//! Manager side of the fd-passing dialer.
//!
//! Why this exists: the manager has to stay in the host network namespace to
//! accept connections, but the app lives in a private namespace. So per service
//! we spawn a tiny helper that owns that namespace. When a client connects, the
//! manager asks the helper to open a socket to `127.0.0.1:<app-port>` inside the
//! namespace and to hand the connected file descriptor back over a Unix socket.
//!
//! The helper's own command line implementation lives in `dialer_child.rs`; the
//! wire format is described in `protocol.rs`.

use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::log::Logger;
use crate::netns::dns::DnsRelay;
use crate::netns::protocol::{
    ANNOUNCE_READY, CONTROL_FD, FIRST_DNS_FD, REPLY_SUCCESS, encode_dial_request,
    encode_route_request,
};
use crate::program;
use crate::sync::lock;
use crate::sys::fd::{inherit_as, recv_with_fd};
use crate::sys::proc::wait_timeout;
use crate::sys::socket::socketpair;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const ROUTE_TIMEOUT: Duration = Duration::from_secs(2);
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// The manager's handle on one helper process.
pub struct Dialer {
    control: Mutex<Option<UnixStream>>,
    pid: i32,
    child: Mutex<Option<Child>>,
}

impl Dialer {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Asks the helper for a connected socket to the app's port.
    pub fn dial(&self, app_port: u16) -> Result<TcpStream> {
        let mut guard = lock(&self.control);
        let Some(control) = guard.as_mut() else {
            anyhow::bail!("the dialer helper is gone");
        };

        let _ = control.set_read_timeout(Some(CONTROL_TIMEOUT));
        let _ = control.set_write_timeout(Some(CONTROL_TIMEOUT));
        let result = self.exchange_dial(control, app_port);
        let _ = control.set_read_timeout(None);
        let _ = control.set_write_timeout(None);
        result
    }

    fn exchange_dial(&self, control: &mut UnixStream, app_port: u16) -> Result<TcpStream> {
        control.write_all(&encode_dial_request(app_port))?;

        let mut status = 0u8;
        match recv_with_fd(control.as_raw_fd(), &mut status)? {
            Some(fd) if status == REPLY_SUCCESS => {
                // SAFETY: the descriptor arrived over SCM_RIGHTS and is ours.
                Ok(crate::sys::fd::adopt_stream(fd)?)
            }
            Some(fd) => {
                crate::sys::fd::close(fd);
                anyhow::bail!("app is not accepting connections")
            }
            None => anyhow::bail!("the dialer helper went away"),
        }
    }

    /// Reports whether the namespace can route traffic to the outside. That
    /// becomes true once the net helper has configured the interface.
    pub fn route_check(&self) -> bool {
        let mut guard = lock(&self.control);
        let Some(control) = guard.as_mut() else {
            return false;
        };

        let _ = control.set_read_timeout(Some(ROUTE_TIMEOUT));
        let result = (|| -> Result<bool> {
            control.write_all(&encode_route_request())?;
            let mut reply = [0u8; 1];
            control.read_exact(&mut reply)?;
            Ok(reply[0] == REPLY_SUCCESS)
        })();
        let _ = control.set_read_timeout(None);
        result.unwrap_or(false)
    }

    /// Closes the control socket (which makes the helper exit) and reaps it.
    pub fn close(&self) {
        drop(lock(&self.control).take());

        let child = lock(&self.child).take();
        let Some(mut child) = child else {
            return;
        };
        match wait_timeout(&mut child, CLOSE_GRACE) {
            Ok(Some(_)) => {}
            _ => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn wait_until_ready(&self) -> Result<()> {
        let mut guard = lock(&self.control);
        let Some(control) = guard.as_mut() else {
            anyhow::bail!("the dialer socket closed before it was ready");
        };

        let _ = control.set_read_timeout(Some(READY_TIMEOUT));
        let mut byte = [0u8; 1];
        let read = control.read_exact(&mut byte);
        let _ = control.set_read_timeout(None);

        match read {
            Ok(()) if byte[0] == ANNOUNCE_READY => Ok(()),
            Ok(()) => Err(anyhow::anyhow!(
                "the dialer did not come up: unexpected byte {:?}",
                byte[0] as char
            )),
            Err(err) => Err(err).context("the dialer did not come up"),
        }
    }
}

/// Spawns the helper, waits until it has brought loopback up, and returns the
/// manager's end of the control socket.
///
/// `relays` are DNS relays whose host-side sockets the helper must inherit; they
/// become fd 4, 5, ... in the child.
pub fn start_dialer(log: &Logger, app_log: Option<&File>, relays: &[DnsRelay]) -> Result<Dialer> {
    let (parent, child_control) = socketpair().context("create the dialer socketpair")?;

    let mut args = vec!["__dial".to_string()];
    let mut pairs = vec![(child_control.as_raw_fd(), CONTROL_FD)];
    for (index, relay) in relays.iter().enumerate() {
        args.push("-dns".to_string());
        args.push(relay.resolver.to_string());
        pairs.push((relay.socket.as_raw_fd(), FIRST_DNS_FD + index as i32));
    }

    let mut command = Command::new(program::self_path());
    command.args(&args);
    command.stdin(Stdio::null());
    command.process_group(0);
    match app_log {
        Some(file) => {
            let stdout = file.try_clone().context("dialer log file")?;
            let stderr = file.try_clone().context("dialer log file")?;
            command.stdout(stdout).stderr(stderr);
        }
        None => {
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }
    }
    inherit_as(&mut command, pairs);

    let child = command.spawn().context("spawn the dialer helper")?;
    // The child owns its copy now.
    drop(child_control);

    let dialer = Dialer {
        control: Mutex::new(Some(parent)),
        pid: child.id() as i32,
        child: Mutex::new(Some(child)),
    };

    if let Err(err) = dialer.wait_until_ready() {
        dialer.close();
        return Err(err);
    }

    crate::debug!(log, "namespace ready", dialer_pid = dialer.pid);
    Ok(dialer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netns::protocol::{REPLY_FAILURE, REQUEST_DIAL, decode_port};
    use crate::sys::fd::send_with_fd;
    use std::net::TcpListener;

    fn bare_dialer(control: UnixStream) -> Dialer {
        Dialer {
            control: Mutex::new(Some(control)),
            pid: 0,
            child: Mutex::new(None),
        }
    }

    #[test]
    fn wire_format_is_stable() {
        let request = encode_dial_request(8080);
        assert_eq!(request[0], REQUEST_DIAL);
        assert_eq!(decode_port(&request), 8080);
    }

    #[test]
    fn ready_announcement_is_required() {
        let (mine, mut theirs) = socketpair().unwrap();
        let dialer = bare_dialer(mine);
        let helper = std::thread::spawn(move || {
            theirs.write_all(&[ANNOUNCE_READY]).unwrap();
        });
        dialer.wait_until_ready().unwrap();
        helper.join().unwrap();

        let (mine, mut theirs) = socketpair().unwrap();
        let dialer = bare_dialer(mine);
        let helper = std::thread::spawn(move || {
            theirs.write_all(b"X").unwrap();
            drop(theirs);
        });
        assert!(dialer.wait_until_ready().is_err());
        helper.join().unwrap();
    }

    #[test]
    fn route_check_reads_a_single_reply_byte() {
        let (mine, mut theirs) = socketpair().unwrap();
        let dialer = bare_dialer(mine);
        let helper = std::thread::spawn(move || {
            let mut request = [0u8; 3];
            theirs.read_exact(&mut request).unwrap();
            assert_eq!(request, encode_route_request());
            theirs.write_all(&[REPLY_SUCCESS]).unwrap();
        });
        assert!(dialer.route_check());
        helper.join().unwrap();
    }

    #[test]
    fn dial_receives_a_connected_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (mine, mut theirs) = socketpair().unwrap();
        let dialer = bare_dialer(mine);

        let helper = std::thread::spawn(move || {
            let mut request = [0u8; 3];
            theirs.read_exact(&mut request).unwrap();
            assert_eq!(decode_port(&request), addr.port());

            let upstream = TcpStream::connect(addr).unwrap();
            let peer = listener.accept().unwrap().0;
            send_with_fd(theirs.as_raw_fd(), REPLY_SUCCESS, upstream.as_raw_fd()).unwrap();
            peer
        });

        let mut stream = dialer.dial(addr.port()).unwrap();
        stream.write_all(b"hi").unwrap();

        let mut peer = helper.join().unwrap();
        let mut buffer = [0u8; 2];
        peer.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"hi");
    }

    #[test]
    fn dial_reports_a_failure_reply() {
        let (mine, mut theirs) = socketpair().unwrap();
        let dialer = bare_dialer(mine);
        let helper = std::thread::spawn(move || {
            let mut request = [0u8; 3];
            theirs.read_exact(&mut request).unwrap();
            theirs.write_all(&[REPLY_FAILURE]).unwrap();
        });
        assert!(dialer.dial(9999).is_err());
        helper.join().unwrap();
    }

    #[test]
    fn a_closed_dialer_reports_errors_instead_of_hanging() {
        let (mine, _theirs) = socketpair().unwrap();
        let dialer = bare_dialer(mine);
        dialer.close();
        assert!(dialer.dial(80).is_err());
        assert!(!dialer.route_check());
    }
}
