//! "Is the app accepting connections yet?"
//!
//! Readiness is deliberately dumb: try every app port, and if any of them
//! refuses, wait a little and try again. The backoff grows from 20 ms to 250 ms
//! so a fast app is noticed immediately and a slow one does not get hammered.
//!
//! Two things end the wait early: the startup timeout, and the process exiting.
//! The second one matters — without it a crashing app would keep us waiting for
//! the full timeout for no reason.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::sync::Latch;

const POLL_INITIAL: Duration = Duration::from_millis(20);
const POLL_CEILING: Duration = Duration::from_millis(250);

/// Polls `dial` until every port in `ports` accepts, the process exits, or
/// `timeout` runs out.
pub fn wait_until_ready(
    dial: impl Fn(u16) -> Result<TcpStream>,
    ports: &[u16],
    timeout: Duration,
    exited: &Latch,
) -> Result<()> {
    let give_up_at = Instant::now() + timeout;
    let mut backoff = POLL_INITIAL;

    loop {
        let pending = ports_that_refuse(&dial, ports);
        if pending.is_empty() {
            return Ok(());
        }
        if exited.is_set() {
            bail!("process exited before becoming ready");
        }
        if Instant::now() >= give_up_at {
            bail!("startup timeout: port(s) {pending:?} never accepted a connection");
        }

        // Sleeping on the latch means a crash wakes us immediately instead of
        // after the backoff has elapsed.
        let wait = backoff.min(give_up_at.saturating_duration_since(Instant::now()));
        let _ = exited.wait_timeout(wait);
        backoff = (backoff * 2).min(POLL_CEILING);
    }
}

fn ports_that_refuse(dial: &impl Fn(u16) -> Result<TcpStream>, ports: &[u16]) -> Vec<u16> {
    let mut pending = Vec::new();
    for port in ports {
        match dial(*port) {
            // The connection is only a probe: readiness means "accepted", and we
            // must not leave it open behind us.
            Ok(connection) => drop(connection),
            Err(_) => pending.push(*port),
        }
    }
    pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    /// A dialer that refuses everything, independent of what is listening on this
    /// machine.
    fn refuse(_port: u16) -> Result<TcpStream> {
        anyhow::bail!("connection refused")
    }

    #[test]
    fn gives_up_when_the_process_exits() {
        let exited = Arc::new(Latch::new());
        let signal = Arc::clone(&exited);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            signal.signal();
        });

        let started = Instant::now();
        let err = wait_until_ready(refuse, &[9000], Duration::from_secs(30), &exited).unwrap_err();
        assert!(format!("{err:#}").contains("process exited"), "{err:#}");
        // Much sooner than the 30 s timeout.
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn times_out_on_a_port_that_never_opens() {
        let exited = Latch::new();
        let err =
            wait_until_ready(refuse, &[9000], Duration::from_millis(200), &exited).unwrap_err();
        assert!(format!("{err:#}").contains("startup timeout"), "{err:#}");
    }

    #[test]
    fn needs_every_port_to_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open_port = listener.local_addr().unwrap().port();
        let closed_port = open_port + 1;
        let exited = Latch::new();

        let err = wait_until_ready(
            move |port| {
                if port == open_port {
                    Ok(TcpStream::connect(("127.0.0.1", port))?)
                } else {
                    refuse(port)
                }
            },
            &[open_port, closed_port],
            Duration::from_millis(200),
            &exited,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains(&closed_port.to_string()),
            "{err:#}"
        );
    }

    #[test]
    fn succeeds_once_the_listener_is_up() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let exited = Latch::new();

        wait_until_ready(
            move |port| Ok(TcpStream::connect(("127.0.0.1", port))?),
            &[port],
            Duration::from_secs(5),
            &exited,
        )
        .unwrap();
    }
}
