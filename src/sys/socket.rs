//! Listening sockets, socketpairs, pipes and `poll`.
//!
//! std can create all of these, but three details matter here and std does not
//! give them to us:
//!
//! * `SO_REUSEADDR` — the launcher wants to be restartable without waiting for
//!   `TIME_WAIT`.
//! * non-blocking listening sockets — the accept loop uses [`poll_readable`] so
//!   it can notice a shutdown request instead of blocking in `accept()` forever.
//! * `SOCK_CLOEXEC`/`O_CLOEXEC` from the start — nothing we create may leak into
//!   a spawned app.

#![allow(unsafe_code)]

use std::io;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::cvt;

/// Binds a listening TCP socket: resolve the address, set `SO_REUSEADDR`, and
/// try every candidate.
pub fn bind_tcp(addr: &str) -> io::Result<TcpListener> {
    let normalized = if addr.starts_with(':') {
        format!("0.0.0.0{addr}")
    } else {
        addr.to_string()
    };

    let candidates: Vec<SocketAddr> = normalized.to_socket_addrs()?.collect();
    if candidates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("{addr}: no addresses"),
        ));
    }

    let mut last_error = None;
    for candidate in candidates {
        match bind_one(candidate) {
            Ok(listener) => return Ok(listener),
            Err(err) => last_error = Some(err),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("{addr}: cannot bind"),
        )
    }))
}

fn bind_one(addr: SocketAddr) -> io::Result<TcpListener> {
    let family = match addr {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };

    // SAFETY: plain socket creation; the descriptor is owned by us until it is
    // wrapped in `TcpListener`.
    let fd = cvt(unsafe {
        libc::socket(
            family,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    })?;

    // Reserve the port for a restart of the launcher itself.
    let enable: libc::c_int = 1;
    // SAFETY: valid descriptor, valid pointer, correct length.
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&enable as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    let (storage, len) = sockaddr(addr);
    // SAFETY: `storage` is a valid sockaddr for the family encoded in it.
    let result =
        cvt(unsafe { libc::bind(fd, (&storage as *const libc::sockaddr_storage).cast(), len) })
            // SAFETY: valid descriptor.
            .and_then(|_| cvt(unsafe { libc::listen(fd, 128) }));

    if let Err(err) = result {
        // SAFETY: we own the descriptor and have not handed it out.
        unsafe { libc::close(fd) };
        return Err(err);
    }

    // SAFETY: a fresh listening TCP socket that we own.
    Ok(unsafe { TcpListener::from_raw_fd(fd) })
}

fn sockaddr(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: sockaddr_in is smaller than sockaddr_storage.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&sin as *const libc::sockaddr_in).cast::<u8>(),
                    (&mut storage as *mut libc::sockaddr_storage).cast::<u8>(),
                    std::mem::size_of::<libc::sockaddr_in>(),
                );
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: v6.scope_id(),
            };
            // SAFETY: sockaddr_in6 is smaller than sockaddr_storage.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&sin6 as *const libc::sockaddr_in6).cast::<u8>(),
                    (&mut storage as *mut libc::sockaddr_storage).cast::<u8>(),
                    std::mem::size_of::<libc::sockaddr_in6>(),
                );
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

/// Creates the two ends of the manager <-> `__dial` control channel.
pub fn socketpair() -> io::Result<(UnixStream, UnixStream)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` points at space for two descriptors.
    cvt(unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    })?;
    // SAFETY: both descriptors are fresh and owned by us.
    unsafe {
        Ok((
            UnixStream::from_raw_fd(fds[0]),
            UnixStream::from_raw_fd(fds[1]),
        ))
    }
}

/// A pipe whose ends are close-on-exec.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` points at space for two descriptors.
    cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: both descriptors are fresh and owned by us.
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

/// Waits until `fd` is readable, at most `timeout`.
///
/// Returns `Ok(false)` on timeout. Waiting in slices is what lets the accept loop
/// notice a shutdown request without any signal tricks: it simply asks every
/// 200 ms whether it should still be running.
pub fn poll_readable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        // SAFETY: one valid pollfd.
        let ready = unsafe {
            libc::poll(
                &mut poll_fd,
                1,
                timeout.as_millis().min(i32::MAX as u128) as libc::c_int,
            )
        };
        if ready == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ready == 0 {
            return Ok(false);
        }
        if poll_fd.revents & libc::POLLIN == 0
            && poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "listener is gone",
            ));
        }
        return Ok(poll_fd.revents & libc::POLLIN != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_and_reports_the_address() {
        let listener = bind_tcp("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(addr.port() > 0);
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert!(listener.set_nonblocking(true).is_ok());
    }

    #[test]
    fn a_port_can_only_be_bound_once_even_with_reuseaddr() {
        let listener = bind_tcp("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        // SO_REUSEADDR allows rebinding a port in TIME_WAIT, not a live listener.
        assert!(bind_tcp(&addr).is_err());
    }

    #[test]
    fn wildcard_addresses_are_accepted() {
        let listener = bind_tcp("0.0.0.0:0").unwrap();
        assert_eq!(listener.local_addr().unwrap().ip().to_string(), "0.0.0.0");
    }

    #[test]
    fn poll_notices_data_and_timeouts() {
        let (mut left, right) = socketpair().unwrap();
        let right_fd = std::os::fd::AsRawFd::as_raw_fd(&right);

        let started = std::time::Instant::now();
        assert!(!poll_readable(right_fd, Duration::from_millis(50)).unwrap());
        assert!(started.elapsed() >= Duration::from_millis(50));

        // Writing to one end makes the other end readable.
        std::io::Write::write_all(&mut left, b"x").unwrap();
        assert!(poll_readable(right_fd, Duration::from_millis(500)).unwrap());
    }
}
