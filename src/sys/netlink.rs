//! Minimal rtnetlink: add an address to an interface.
//!
//! Why not the old SIOCSIFADDR ioctl? That replaces the interface's primary
//! address, and on `lo` it would drop 127.0.0.1/8. RTM_NEWADDR is additive and
//! behaves like `ip addr add`.
//!
//! Wire format (all little-endian):
//!
//! ```text
//! nlmsghdr  16 bytes  length, type, flags, seq, pid
//! ifaddrmsg  8 bytes  family, prefixlen, flags, scope, interface index
//! rtattr...           TLV attributes, padded to 4 bytes each
//! ```

#![allow(unsafe_code)]

use std::io;
use std::net::Ipv4Addr;

use super::{cvt, cvt_syscall};
use crate::sys::ns::if_index;

const RTM_NEWADDR: u16 = 20;

const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_EXCL: u16 = 0x200;

const NLMSG_ERROR: u16 = 2;
const RT_SCOPE_HOST: u8 = 254;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;

const NLMSG_HEADER_SIZE: usize = 16;

/// Adds `ip/prefixlen` to the named interface, like `ip addr add`.
pub fn add_addr(ifname: &str, ip: Ipv4Addr, prefixlen: u8) -> io::Result<()> {
    let index = if_index(ifname)?;
    let message = new_addr_message(index, ip, prefixlen);
    send_netlink_request(&message)
}

/// Builds one RTM_NEWADDR request. Kept separate so it can be tested without
/// touching the kernel.
pub fn new_addr_message(index: i32, ip: Ipv4Addr, prefixlen: u8) -> Vec<u8> {
    let mut message = Vec::with_capacity(64);
    message.extend_from_slice(&[0u8; NLMSG_HEADER_SIZE]); // room for nlmsghdr

    // ifaddrmsg
    message.push(libc::AF_INET as u8);
    message.push(prefixlen);
    message.push(0); // flags
    message.push(RT_SCOPE_HOST);
    message.extend_from_slice(&(index as u32).to_le_bytes());

    message = append_attr(message, IFA_LOCAL, &ip.octets());
    message = append_attr(message, IFA_ADDRESS, &ip.octets());

    // Fill in the header now that the length is known.
    let length = message.len() as u32;
    message[0..4].copy_from_slice(&length.to_le_bytes());
    message[4..6].copy_from_slice(&RTM_NEWADDR.to_le_bytes());
    message[6..8]
        .copy_from_slice(&(NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL).to_le_bytes());
    message[8..12].copy_from_slice(&1u32.to_le_bytes()); // sequence number
    message
}

fn append_attr(mut message: Vec<u8>, kind: u16, data: &[u8]) -> Vec<u8> {
    let mut padded = data.to_vec();
    while !padded.len().is_multiple_of(4) {
        padded.push(0);
    }

    let length = (4 + padded.len()) as u16;
    message.extend_from_slice(&length.to_le_bytes());
    message.extend_from_slice(&kind.to_le_bytes());
    message.extend_from_slice(&padded);
    message
}

/// Sends one request and waits for the kernel's ACK.
fn send_netlink_request(message: &[u8]) -> io::Result<()> {
    // SAFETY: plain netlink socket creation; the fd is closed below.
    let fd = cvt(unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    })?;

    let result = (|| {
        // SAFETY: sockaddr_nl is a plain C struct of integers; zero is a valid
        // value for every field (nl_pad is private, so build it this way).
        let mut to: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        to.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: message and sockaddr are valid for the duration of the call.
        cvt_syscall(unsafe {
            libc::sendto(
                fd,
                message.as_ptr().cast(),
                message.len(),
                0,
                (&to as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        })?;

        let mut reply = [0u8; 4096];
        // SAFETY: reply is a valid buffer of `reply.len()` bytes.
        let received =
            cvt_syscall(unsafe { libc::recv(fd, reply.as_mut_ptr().cast(), reply.len(), 0) })?;

        if (received as usize) < NLMSG_HEADER_SIZE {
            return Err(io::Error::other("netlink: short reply"));
        }
        let kind = u16::from_le_bytes([reply[4], reply[5]]);
        if kind != NLMSG_ERROR {
            return Ok(()); // not an error message: success
        }

        // NLMSG_ERROR payload starts right after the 16-byte header.
        let code = i32::from_le_bytes([reply[16], reply[17], reply[18], reply[19]]);
        if code == 0 {
            return Ok(());
        }
        Err(io::Error::from_raw_os_error(-code))
    })();

    // SAFETY: we own fd and do not use it afterwards.
    unsafe { libc::close(fd) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_layout_matches_rtnetlink() {
        let message = new_addr_message(1, Ipv4Addr::new(127, 0, 0, 53), 32);

        // nlmsghdr: length, type, flags, seq, pid
        let length = u32::from_le_bytes(message[0..4].try_into().unwrap());
        assert_eq!(length as usize, message.len());
        assert_eq!(
            u16::from_le_bytes(message[4..6].try_into().unwrap()),
            RTM_NEWADDR
        );
        assert_eq!(
            u16::from_le_bytes(message[6..8].try_into().unwrap()),
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL
        );
        assert_eq!(u32::from_le_bytes(message[8..12].try_into().unwrap()), 1);

        // ifaddrmsg
        assert_eq!(message[16], libc::AF_INET as u8);
        assert_eq!(message[17], 32); // prefix length
        assert_eq!(message[18], 0); // flags
        assert_eq!(message[19], RT_SCOPE_HOST);
        assert_eq!(u32::from_le_bytes(message[20..24].try_into().unwrap()), 1);

        // IFA_LOCAL
        assert_eq!(u16::from_le_bytes(message[24..26].try_into().unwrap()), 8);
        assert_eq!(
            u16::from_le_bytes(message[26..28].try_into().unwrap()),
            IFA_LOCAL
        );
        assert_eq!(&message[28..32], &[127, 0, 0, 53]);

        // IFA_ADDRESS
        assert_eq!(u16::from_le_bytes(message[32..34].try_into().unwrap()), 8);
        assert_eq!(
            u16::from_le_bytes(message[34..36].try_into().unwrap()),
            IFA_ADDRESS
        );
        assert_eq!(&message[36..40], &[127, 0, 0, 53]);

        assert_eq!(message.len(), 40);
    }

    #[test]
    fn attributes_are_padded_to_four_bytes() {
        let message = append_attr(Vec::new(), IFA_LOCAL, &[1, 2, 3]);
        assert_eq!(message.len(), 8); // 4-byte header + 3 bytes + 1 pad
        assert_eq!(u16::from_le_bytes(message[0..2].try_into().unwrap()), 8);
        assert_eq!(message[7], 0);
    }

    #[test]
    fn unknown_interface_is_reported() {
        let err = add_addr("nope-no-such-if", Ipv4Addr::new(10, 0, 0, 1), 32).unwrap_err();
        assert!(err.to_string().contains("nope-no-such-if") || err.raw_os_error().is_some());
    }
}
