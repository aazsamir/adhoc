//! Outbound network for an app in its own namespace.
//!
//! An unprivileged user namespace cannot attach a veth to the host, so giving
//! the namespace a route to the outside needs a userspace network stack:
//!
//! * `pasta` — the modern choice (passt). Copies the host's addresses and routes
//!   into the namespace and splices TCP/UDP in userspace, so the app's own
//!   /etc/resolv.conf keeps working.
//! * `slirp4netns` — the older alternative, with its own 10.0.2.0/24 subnet.
//! * `custom` — whatever command the user configured.
//!
//! The helper is attached to the namespace *before* the app starts, and it is
//! killed together with the app. It is not required for loopback-only services.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{NetMode, ServiceConfig};
use crate::log::Logger;
use crate::program;
use crate::sync::Latch;
use crate::sys::fd::inherit_as;
use crate::sys::proc::{kill_group, terminate_group, wait_timeout};
use crate::sys::socket::pipe;
use crate::tailwriter::TailWriter;

const SLIRP_READY_FD: i32 = 3;
const HELPER_LOG_TAIL_LINES: usize = 8;
const HELPER_STARTUP_GRACE: Duration = Duration::from_millis(150);
const HELPER_STOP_GRACE: Duration = Duration::from_secs(3);
const HELPER_READY_TIMEOUT: Duration = Duration::from_secs(10);
const HELPER_DEATH_WINDOW: Duration = Duration::from_millis(700);

/// One running outbound-network helper.
pub struct NetHelper {
    name: String,
    pid: i32,
    done: Arc<Latch>,
    stopping: Arc<AtomicBool>,
    /// The last lines the helper printed, for the "it died" explanation.
    tail: Arc<TailWriter>,
}

impl NetHelper {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Terminates the helper politely, then forcefully.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if self.done.is_set() {
            return;
        }

        terminate_group(self.pid);
        if !self.done.wait_timeout(HELPER_STOP_GRACE) {
            kill_group(self.pid);
            let _ = self.done.wait_timeout(HELPER_STOP_GRACE);
        }
    }

    /// Reaps the helper in the background and explains a helper that dies on its
    /// own: outbound network silently disappearing is worth a log line.
    fn watch(&self, mut child: Child, log: &Logger) {
        let name = self.name.clone();
        let done = Arc::clone(&self.done);
        let stopping = Arc::clone(&self.stopping);
        let tail = Arc::clone(&self.tail);
        let log = log.clone();
        let started = Instant::now();

        std::thread::spawn(move || {
            let status = child.wait();
            done.signal();

            if stopping.load(Ordering::SeqCst) {
                return; // we asked it to stop
            }
            let description = match &status {
                Ok(status) => status.to_string(),
                Err(err) => err.to_string(),
            };

            if started.elapsed() < HELPER_DEATH_WINDOW {
                log.error(
                    "network helper died",
                    &[("helper", name.clone()), ("err", description)],
                );
                for line in tail.tail() {
                    log.error(&format!("  helper said: {line}"), &[]);
                }
                return;
            }
            log.warn(
                "network helper exited, outbound network is gone; the app keeps running",
                &[("helper", name), ("err", description)],
            );
        });
    }
}

/// Starts the outbound-network helper for a namespace, or returns `Ok(None)`
/// when the service has no outbound network.
pub fn start_net_helper(
    cfg: &ServiceConfig,
    netns_pid: i32,
    app_log: Option<&File>,
    log: &Logger,
) -> Result<Option<NetHelper>> {
    if cfg.net_mode() == NetMode::None {
        return Ok(None);
    }

    let netns_path = format!("/proc/{netns_pid}/ns/net");
    let argv = net_argv(cfg, &netns_path)?;
    if !program::exists(&argv[0]) {
        bail!(
            "{} not found in PATH - install it, or use net: none",
            argv[0]
        );
    }

    // One pipe for both stdout and stderr keeps the helper's own interleaving.
    let (output_read, output_write) = pipe().context("create the helper output pipe")?;
    // slirp4netns reports readiness by writing one byte to fd 3.
    let (ready_read, ready_write) = if cfg.net_mode() == NetMode::Slirp4netns {
        let (read, write) = pipe().context("create the readiness pipe")?;
        (Some(read), Some(write))
    } else {
        (None, None)
    };

    let mut pairs = vec![(output_write.as_raw_fd(), 1), (output_write.as_raw_fd(), 2)];
    if let Some(write) = &ready_write {
        pairs.push((write.as_raw_fd(), SLIRP_READY_FD));
    }

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.stdin(Stdio::null());
    command.process_group(0);
    inherit_as(&mut command, pairs);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {}", argv[0]))?;
    drop(output_write);
    drop(ready_write);

    let tail = Arc::new(TailWriter::new(HELPER_LOG_TAIL_LINES));
    spawn_output_reader(output_read, app_log, tail.clone())?;

    let pid = child.id() as i32;
    if let Some(ready) = ready_read
        && let Err(err) = wait_for_slirp_ready(ready)
    {
        kill_group(pid);
        let _ = wait_timeout(&mut child, HELPER_STOP_GRACE);
        return Err(err).with_context(|| format!("{} did not become ready", argv[0]));
    }

    crate::debug!(
        log,
        "network helper started",
        helper = argv[0],
        argv = argv.join(" ")
    );

    let helper = NetHelper {
        name: argv[0].clone(),
        pid,
        done: Arc::new(Latch::new()),
        stopping: Arc::new(AtomicBool::new(false)),
        tail,
    };
    helper.watch(child, log);

    // A helper that dies instantly is a configuration problem: report it now
    // instead of letting the app fail mysteriously later.
    if helper.done.wait_timeout(HELPER_STARTUP_GRACE) {
        bail!("{} exited immediately", argv[0]);
    }
    Ok(Some(helper))
}

/// Forwards the helper's output to the service's log and remembers its tail.
fn spawn_output_reader(read: OwnedFd, app_log: Option<&File>, tail: Arc<TailWriter>) -> Result<()> {
    let mut sink: Box<dyn Write + Send> = match app_log {
        Some(file) => Box::new(file.try_clone().context("clone the helper log file")?),
        None => Box::new(io::stdout()),
    };

    std::thread::spawn(move || {
        let mut reader = File::from(read);
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(length) => {
                    let _ = tail.forward(&mut *sink, &buffer[..length]);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    });
    Ok(())
}

/// Blocks until slirp4netns has configured the namespace.
fn wait_for_slirp_ready(read: OwnedFd) -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut file = File::from(read);
        let mut buffer = [0u8; 8];
        let _ = sender.send(file.read(&mut buffer));
    });

    match receiver.recv_timeout(HELPER_READY_TIMEOUT) {
        Ok(Ok(length)) if length > 0 => Ok(()),
        Ok(Ok(_)) => bail!("the helper closed its readiness pipe"),
        Ok(Err(err)) => Err(err).context("read the readiness byte"),
        Err(_) => bail!("timed out waiting for the readiness byte"),
    }
}

/// Builds the helper invocation for a net mode. `{netns}` is the path to the
/// target namespace (fd 3 is reserved for slirp4netns' readiness report).
pub fn net_argv(cfg: &ServiceConfig, netns_path: &str) -> Result<Vec<String>> {
    let extra = || cfg.net_args.iter().cloned();

    match cfg.net_mode() {
        NetMode::Pasta => {
            // -T none -U none matter: pasta's defaults ("auto") re-bind the
            // host's listening ports *inside* the namespace so the app can
            // reach host services. That collides with the port the proxy itself
            // holds, and the app then dies with "address already in use".
            let mut argv: Vec<String> = [
                "pasta",
                "-f",
                "--config-net",
                "--quiet",
                "-T",
                "none",
                "-U",
                "none",
                "--netns",
                netns_path,
            ]
            .iter()
            .map(|part| part.to_string())
            .collect();
            argv.extend(extra());
            Ok(argv)
        }

        NetMode::Slirp4netns => {
            let mut argv: Vec<String> = [
                "slirp4netns",
                "--configure",
                "--mtu=65520",
                "--netns-type=path",
                "-r",
                "3",
                netns_path,
            ]
            .iter()
            .map(|part| part.to_string())
            .collect();
            argv.extend(extra());
            Ok(argv)
        }

        NetMode::Custom => {
            if cfg.net_command.is_empty() {
                bail!("net: custom requires net_command");
            }
            Ok(cfg
                .net_command
                .iter()
                .map(|arg| {
                    arg.replace("{netns}", netns_path)
                        .replace("{pid}", &std::process::id().to_string())
                })
                .collect())
        }

        NetMode::None => bail!("net: none needs no helper"),
    }
}

/// Blocks until the namespace has a route to the outside, i.e. until the helper
/// has configured the interface. A missing route is only a warning: the app may
/// still work on loopback.
pub fn wait_network_ready(
    check: impl Fn() -> bool,
    timeout: Duration,
    helper: &str,
    log: &Logger,
) -> bool {
    let started = Instant::now();
    loop {
        if check() {
            crate::info!(
                log,
                "outbound network ready",
                helper = helper,
                took = crate::config::Duration::from(started.elapsed())
            );
            return true;
        }
        if started.elapsed() >= timeout {
            crate::warn!(
                log,
                "no route to the outside yet, starting the app anyway",
                helper = helper
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(net: &str) -> ServiceConfig {
        ServiceConfig {
            name: "a".to_string(),
            net: net.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn pasta_argv_disables_automatic_port_forwarding() {
        let argv = net_argv(&svc("pasta"), "/proc/1234/ns/net").unwrap();
        let joined = argv.join(" ");
        for expected in [
            "pasta",
            "-f",
            "--config-net",
            "--netns /proc/1234/ns/net",
            "-T none",
            "-U none",
        ] {
            assert!(joined.contains(expected), "{joined} missing {expected}");
        }
    }

    #[test]
    fn slirp_argv_uses_fd_three_for_readiness() {
        let mut cfg = svc("slirp4netns");
        cfg.net_args = vec!["--enable-ipv6".to_string()];
        let argv = net_argv(&cfg, "/proc/1234/ns/net").unwrap();
        let joined = argv.join(" ");
        for expected in [
            "slirp4netns",
            "--configure",
            "-r 3",
            "/proc/1234/ns/net",
            "--enable-ipv6",
        ] {
            assert!(joined.contains(expected), "{joined} missing {expected}");
        }
    }

    #[test]
    fn custom_argv_expands_placeholders() {
        let mut cfg = svc("custom");
        assert!(net_argv(&cfg, "/proc/1/ns/net").is_err());

        cfg.net_command = vec!["mynet".to_string(), "{netns}".to_string()];
        let argv = net_argv(&cfg, "/proc/1/ns/net").unwrap();
        assert_eq!(argv, vec!["mynet", "/proc/1/ns/net"]);
    }

    #[test]
    fn none_has_no_helper() {
        assert!(net_argv(&svc("none"), "/proc/1/ns/net").is_err());
    }
}
