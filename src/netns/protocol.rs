//! The manager and its per-namespace dialer helper talk over a Unix socketpair.
//! The protocol is deliberately tiny: every request is exactly three bytes and
//! every reply is exactly one byte (plus a file descriptor where noted).
//!
//! ```text
//! manager -> helper                    helper -> manager
//! ['D', portHi, portLo]  dial          [1] + fd via SCM_RIGHTS, or [0]
//! ['N', 0, 0]            route check   [1] = yes, [0] = no
//! ```
//!
//! Before any of that the helper sends a single 'R' byte to say that loopback
//! is up and it is ready. File descriptors are plain integers in the child:
//! fd 3 is the socketpair, fd 4 and up are the host-side DNS sockets.

use std::os::fd::RawFd;

pub const REQUEST_DIAL: u8 = b'D';
pub const REQUEST_ROUTE: u8 = b'N';
pub const ANNOUNCE_READY: u8 = b'R';

pub const REPLY_FAILURE: u8 = 0;
pub const REPLY_SUCCESS: u8 = 1;

/// All requests are three bytes.
pub const REQUEST_SIZE: usize = 3;

/// Child side of the socketpair.
pub const CONTROL_FD: RawFd = 3;
/// First host-side DNS socket.
pub const FIRST_DNS_FD: RawFd = 4;

pub fn encode_dial_request(port: u16) -> [u8; REQUEST_SIZE] {
    [REQUEST_DIAL, (port >> 8) as u8, port as u8]
}

pub fn encode_route_request() -> [u8; REQUEST_SIZE] {
    [REQUEST_ROUTE, 0, 0]
}

pub fn decode_port(request: &[u8; REQUEST_SIZE]) -> u16 {
    u16::from_be_bytes([request[1], request[2]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dial_requests_round_trip() {
        for port in [1u16, 80, 8080, 65535] {
            let request = encode_dial_request(port);
            assert_eq!(request[0], REQUEST_DIAL);
            assert_eq!(decode_port(&request), port);
        }
    }

    #[test]
    fn small_ports_are_big_endian() {
        assert_eq!(encode_dial_request(8080), [b'D', 0x1f, 0x90]);
    }

    #[test]
    fn route_request_has_no_port() {
        assert_eq!(encode_route_request(), [b'N', 0, 0]);
    }
}
