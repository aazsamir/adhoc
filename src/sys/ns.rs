//! Low-level Linux namespace plumbing: unshare, setns, bring `lo` up, and the
//! route probe.
//!
//! Namespaces belong to a *thread*, not a process. Every caller here runs on the
//! main thread before any other thread exists, which is why plain libc calls are
//! enough: no thread pinning, no runtime cooperation.

#![allow(unsafe_code)]

use std::io;
use std::net::{Ipv4Addr, UdpSocket};

use super::cvt;

const IFNAMSIZ: usize = 16;
const IFREQ_SIZE: usize = 40; // struct ifreq on 64-bit Linux

/// Moves the calling thread into a brand new, empty network namespace.
pub fn new_netns() -> io::Result<()> {
    // SAFETY: unshare(CLONE_NEWNET) on an unshared, single-threaded process.
    cvt(unsafe { libc::unshare(libc::CLONE_NEWNET) })?;
    Ok(())
}

/// Moves the calling thread into the network namespace of `pid`.
pub fn enter_netns(pid: i32) -> io::Result<()> {
    let file = std::fs::File::open(format!("/proc/{pid}/ns/net"))?;
    // SAFETY: the fd is an open namespace handle owned by `file`.
    cvt(unsafe { libc::setns(std::os::fd::AsRawFd::as_raw_fd(&file), libc::CLONE_NEWNET) })?;
    Ok(())
}

/// Brings "lo" up in the current network namespace, using plain ioctls so no
/// external tool (`ip`, `ifconfig`) is needed.
pub fn up_loopback() -> io::Result<()> {
    // SAFETY: plain socket creation; the fd is closed below.
    let fd = cvt(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;

    let result = configure_loopback(fd);
    // SAFETY: we own fd and do not use it afterwards.
    unsafe { libc::close(fd) };
    result
}

fn configure_loopback(fd: libc::c_int) -> io::Result<()> {
    let mut request = [0u8; IFREQ_SIZE];
    request[..2].copy_from_slice(b"lo");

    // SAFETY: SIOCGIFFLAGS fills the ifreq we passed.
    cvt(unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut request as *mut _) })?;

    // The flags live at byte offset 16 in struct ifreq.
    let flags = u16::from_ne_bytes([request[16], request[17]])
        | libc::IFF_UP as u16
        | libc::IFF_RUNNING as u16;
    request[16..18].copy_from_slice(&flags.to_ne_bytes());

    // SAFETY: SIOCSIFFLAGS reads the same ifreq layout.
    cvt(unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &request as *const _) })?;
    Ok(())
}

/// Reports whether the namespace has a route towards `probe`.
///
/// A UDP connect is enough: it succeeds without sending a packet as soon as a
/// route exists, and fails when there is none.
pub fn route_exists(probe: &str) -> bool {
    match probe.parse::<std::net::SocketAddr>() {
        Ok(addr) => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|socket| socket.connect(addr))
            .is_ok(),
        Err(_) => false,
    }
}

/// Resolves an interface name to its kernel index (`SIOCGIFINDEX`).
pub fn if_index(name: &str) -> io::Result<i32> {
    if name.len() >= IFNAMSIZ {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("interface name {name:?} is too long"),
        ));
    }

    // SAFETY: plain socket creation; the fd is closed below.
    let fd = cvt(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;

    let mut request = [0u8; IFREQ_SIZE];
    request[..name.len()].copy_from_slice(name.as_bytes());

    // SAFETY: SIOCGIFINDEX fills the ifreq we passed.
    let result = cvt(unsafe { libc::ioctl(fd, libc::SIOCGIFINDEX as _, &mut request as *mut _) });
    // SAFETY: we own fd and do not use it afterwards.
    unsafe { libc::close(fd) };

    result?;
    // The index lives at byte offset 16 in struct ifreq.
    Ok(i32::from_ne_bytes([
        request[16],
        request[17],
        request[18],
        request[19],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_interface_has_an_index() {
        let index = if_index("lo").unwrap();
        assert!(index > 0);
    }

    #[test]
    fn unknown_interfaces_fail() {
        assert!(if_index("definitely-not-an-interface").is_err());
    }

    #[test]
    fn route_probe_does_not_panic_on_garbage() {
        assert!(!route_exists("not an address"));
    }

    #[test]
    fn entering_a_missing_namespace_fails() {
        assert!(enter_netns(4_194_303).is_err());
    }
}
