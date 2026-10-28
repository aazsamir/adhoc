//! Helper process side of the fd-passing dialer (see `dialer.rs`).
//!
//! The helper is started by `start_dialer` as `adhoc __dial ...`, inheriting the
//! control socketpair as fd 3. It unshares a fresh network namespace, brings
//! loopback up and then serves dial requests until the manager closes the
//! socket.
//!
//! Everything here runs on the main thread before any other thread exists, so
//! `unshare(CLONE_NEWNET)` is legal: the kernel refuses it for multithreaded
//! callers.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::cli::Args;
use crate::log::Logger;
use crate::netns::dns;
use crate::netns::protocol::{
    ANNOUNCE_READY, CONTROL_FD, FIRST_DNS_FD, REPLY_FAILURE, REPLY_SUCCESS, REQUEST_ROUTE,
    REQUEST_SIZE, decode_port,
};
use crate::sys::fd::{adopt_unix_stream, send_with_fd};
use crate::sys::ns;

const DIAL_TIMEOUT: Duration = Duration::from_secs(3);

pub fn cmd_dial(args: &[String]) -> i32 {
    let parsed = Args::parse(args, &["dns"]);
    let dns_specs: Vec<String> = parsed
        .flags
        .iter()
        .filter(|(flag, _)| flag == "dns")
        .filter_map(|(_, value)| value.clone())
        .collect();

    match run(&dns_specs) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("__dial: {err:#}");
            1
        }
    }
}

/// Becomes the namespace owner, then answers requests until the manager leaves.
///
/// Order matters: `unshare` must happen while this process still has a single
/// thread, and the namespace's loopback must be up before anything is dialled.
fn run(dns_specs: &[String]) -> Result<()> {
    ns::new_netns().context("unshare(CLONE_NEWNET)")?;
    ns::up_loopback().context("bring lo up in the new namespace")?;

    start_dns_relays(dns_specs, &Logger::new(false));

    let mut control = adopt_unix_stream(CONTROL_FD).context("control socket on fd 3")?;
    control
        .write_all(&[ANNOUNCE_READY])
        .context("announce readiness to the manager")?;

    serve_dial_requests(&mut control);
    Ok(())
}

/// Relays every `-dns` resolver the manager passed in. The host-side sockets
/// arrive as fd 4, 5, ...
fn start_dns_relays(specs: &[String], log: &Logger) {
    for (index, spec) in specs.iter().enumerate() {
        let Ok(resolver) = spec.parse::<Ipv4Addr>() else {
            continue;
        };
        let fd = FIRST_DNS_FD + index as RawFd;
        let host = match dns::relay_from_fd(fd) {
            Ok(socket) => socket,
            Err(err) => {
                dns::report_relay_error(resolver, &err.to_string());
                continue;
            }
        };
        if let Err(err) = dns::serve_dns_relay(host, resolver, log.clone()) {
            dns::report_relay_error(resolver, &err.to_string());
        }
    }
}

/// Answers the manager's three-byte requests until the socket is closed.
fn serve_dial_requests(control: &mut UnixStream) {
    let mut request = [0u8; REQUEST_SIZE];
    loop {
        if control.read_exact(&mut request).is_err() {
            return; // the manager is gone
        }

        if request[0] == REQUEST_ROUTE {
            // A UDP connect succeeds as soon as a route exists and sends no
            // packets, which is exactly the readiness question we are asking.
            let reply = if ns::route_exists("1.1.1.1:53") {
                REPLY_SUCCESS
            } else {
                REPLY_FAILURE
            };
            if control.write_all(&[reply]).is_err() {
                return;
            }
            continue;
        }

        dial_and_pass_socket(control, decode_port(&request));
    }
}

/// Connects to the app inside this namespace and passes the connected socket to
/// the manager, or reports failure with a single zero byte.
fn dial_and_pass_socket(control: &mut UnixStream, port: u16) {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(stream) = TcpStream::connect_timeout(&address, DIAL_TIMEOUT) else {
        let _ = control.write_all(&[REPLY_FAILURE]);
        return;
    };

    if send_with_fd(control.as_raw_fd(), REPLY_SUCCESS, stream.as_raw_fd()).is_err() {
        return; // manager is gone
    }
    drop(stream); // our copy is no longer needed
}
