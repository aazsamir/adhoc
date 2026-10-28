//! The rootless user-namespace re-exec.
//!
//! # Why this exists
//!
//! A private network namespace is only useful if the app inside it can be given
//! its own loopback, and creating one needs `CLONE_NEWNET`, which needs
//! privileges. Unprivileged users get them from a *user* namespace.
//!
//! The kernel refuses `unshare(CLONE_NEWUSER)` when the caller has more than one
//! thread, and by the time we have bound the listen sockets this process is
//! multithreaded. So we do what runc does: ask util-linux `unshare` for exactly
//! that one step and re-execute ourselves inside it.
//!
//! # How the sockets get there
//!
//! `unshare` cannot bind ports for us, so the parent binds them *first* and
//! passes the descriptors along:
//!
//! ```text
//! adhoc run --config x.yaml                     (binds 127.0.0.1:8080 -> fd 5)
//!   └── unshare --user --map-root-user -- adhoc __serve ... --map 3:0:8080
//!         └── child: fd 3 is the listener for service 0, app port 8080
//! ```
//!
//! The `--map` argument is that sentence in a machine-readable form:
//! `<fd>:<service index>:<app port>,...`. Both directions live here, so the two
//! halves can be tested against each other.

use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::app::ReservedListener;
use crate::log::Logger;
use crate::program;
use crate::sys::fd::{adopt_listener, inherit_as};
use crate::sys::proc;

/// The first descriptor number the child sees the listeners on.
const FIRST_INHERITED_FD: RawFd = 3;
/// How often the supervisor checks whether the child or a signal arrived.
const SUPERVISOR_POLL: Duration = Duration::from_millis(20);

/// Re-executes this binary inside a user namespace and supervises it.
///
/// Returns the child's exit code. Signals are forwarded, so `Ctrl-C` reaches the
/// launcher that actually owns the services.
pub fn restart(
    listeners: Vec<ReservedListener>,
    config_path: &str,
    debug: bool,
    log: &Logger,
) -> Result<i32> {
    let (pairs, mapping) = encode(&listeners);

    let mut command = Command::new("unshare");
    command.args(["--user", "--map-root-user", "--"]);
    command.arg(program::self_path());
    command.args(["__serve", "--config", config_path]);
    if debug {
        command.arg("--debug");
    }
    command.args(["--map", &mapping]);
    command.stdin(Stdio::inherit()).stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    inherit_as(&mut command, pairs);

    let mut child = command
        .spawn()
        .context("start `unshare --user` (is util-linux installed?)")?;
    // The child has its own descriptors now; the ports stay bound through them.
    drop(listeners);

    crate::debug!(log, "re-executed in a user namespace", pid = child.id());

    supervise(&mut child)
}

/// Waits for the child, forwarding shutdown signals to it.
fn supervise(child: &mut Child) -> Result<i32> {
    loop {
        if let Some(status) = child.try_wait().context("wait for the child")? {
            return Ok(proc::exit_code(status));
        }
        if let Some(signal) = proc::take_shutdown_signal() {
            proc::forward_signal(child.id(), signal);
        }
        thread::sleep(SUPERVISOR_POLL);
    }
}

/// Turns listeners into `(source fd, target fd)` pairs plus the `--map` string.
fn encode(listeners: &[ReservedListener]) -> (Vec<(RawFd, RawFd)>, String) {
    let mut pairs = Vec::with_capacity(listeners.len());
    let mut entries = Vec::with_capacity(listeners.len());

    for (index, listener) in listeners.iter().enumerate() {
        let target = FIRST_INHERITED_FD + index as RawFd;
        pairs.push((listener.listener().as_raw_fd(), target));
        entries.push(format!(
            "{target}:{}:{}",
            listener.service(),
            listener.app_port()
        ));
    }
    (pairs, entries.join(","))
}

/// Restores listeners from a `--map` spec handed over by [`restart`].
pub fn listeners_from_fds(spec: &str) -> Result<Vec<ReservedListener>> {
    spec.split(',')
        .map(|entry| {
            let (fd, service, app_port) = parse_entry(entry)?;
            let listener =
                adopt_listener(fd).with_context(|| format!("fd {fd} was not inherited"))?;
            let addr = listener
                .local_addr()
                .map(|addr| addr.to_string())
                .unwrap_or_default();
            Ok(ReservedListener::new(service, addr, app_port, listener))
        })
        .collect()
}

fn parse_entry(entry: &str) -> Result<(RawFd, usize, u16)> {
    let fields: Vec<&str> = entry.split(':').collect();
    let [fd, service, app_port] = fields[..] else {
        bail!("bad --map entry {entry:?}");
    };
    Ok((
        fd.parse()
            .with_context(|| format!("bad --map entry {entry:?}"))?,
        service
            .parse()
            .with_context(|| format!("bad --map entry {entry:?}"))?,
        app_port
            .parse()
            .with_context(|| format!("bad --map entry {entry:?}"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::socket::bind_tcp;

    #[test]
    fn the_map_round_trips() {
        let first = bind_tcp("127.0.0.1:0").unwrap();
        let second = bind_tcp("127.0.0.1:0").unwrap();

        let listeners = vec![
            ReservedListener::new(0, "127.0.0.1:1".to_string(), 8080, first),
            ReservedListener::new(2, "127.0.0.1:2".to_string(), 9090, second),
        ];

        let (pairs, mapping) = encode(&listeners);
        assert_eq!(mapping, "3:0:8080,4:2:9090", "descriptor numbers are fixed");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].1, 3);
        assert_eq!(pairs[1].1, 4);

        // Decoding adopts whatever descriptor numbers the spec names, which after
        // a real re-exec are 3 and 4. Inside a test we cannot control the numbers
        // our own sockets got, so feed the mapping those instead: this still
        // exercises the decoding half.
        let owned = format!("{}:0:8080,{}:2:9090", pairs[0].0, pairs[1].0);
        std::mem::forget(listeners); // the descriptors now live in `pairs`

        let restored = listeners_from_fds(&owned).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].service(), 0);
        assert_eq!(restored[0].app_port(), 8080);
        assert_eq!(restored[1].service(), 2);
        assert_eq!(restored[1].app_port(), 9090);
        for listener in &restored {
            assert!(listener.addr().starts_with("127.0.0.1:"));
            assert!(crate::sys::fd::is_cloexec(listener.listener().as_raw_fd()));
        }
    }

    #[test]
    fn bad_map_entries_are_rejected() {
        for spec in ["nonsense", "1:2", "x:2:3", "1:2:99999", "999999:0:1"] {
            assert!(listeners_from_fds(spec).is_err(), "{spec}");
        }
    }
}
