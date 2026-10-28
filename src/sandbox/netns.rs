//! `isolation: netns` — the app gets its own network namespace.
//!
//! The app can bind `127.0.0.1:8080` even while the proxy holds
//! `127.0.0.1:8080` on the host: they are different kernel objects in different
//! namespaces.
//!
//! This file is *glue only*. It starts three things and keeps them together,
//! and each of them is explained where it is implemented:
//!
//! 1. the [`dialer`] helper, which owns the namespace and hands connections back,
//! 2. the optional outbound-network helper ([`nethelper`]), so the app can reach
//!    the outside world,
//! 3. the optional DNS relays ([`dns`]), so the app's own `/etc/resolv.conf`
//!    keeps working.
//!
//! The manager itself never enters the namespace. On every connection it asks
//! the dialer for a connected socket and then splices bytes.

use std::fs::File;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::config::{NetMode, ServiceConfig};
use crate::log::Logger;
use crate::netns::dialer::{Dialer, start_dialer};
use crate::netns::dns::{DnsRelay, prepare_dns_relays, resolve_dns_targets};
use crate::netns::nethelper::{NetHelper, start_net_helper, wait_network_ready};
use crate::program;
use crate::sandbox::Sandbox;
use crate::sync::lock;

pub struct NetnsSandbox {
    cfg: ServiceConfig,
    log: Logger,
    /// Everything that exists only while the app is running.
    running: Mutex<Option<Running>>,
}

/// The helpers that live as long as the app does.
struct Running {
    dialer: Arc<Dialer>,
    net_helper: Option<NetHelper>,
    /// The manager's copies of the DNS sockets. The helper has its own; we keep
    /// these so the descriptors stay alive.
    relays: Vec<DnsRelay>,
}

impl NetnsSandbox {
    pub fn new(cfg: ServiceConfig, log: Logger) -> NetnsSandbox {
        NetnsSandbox {
            cfg,
            log,
            running: Mutex::new(None),
        }
    }

    fn start_dns_relays(&self) -> Vec<DnsRelay> {
        if self.cfg.net_mode() == NetMode::None {
            return Vec::new();
        }
        match prepare_dns_relays(&resolve_dns_targets(&self.cfg, &self.log)) {
            Ok(relays) => relays,
            Err(err) => {
                crate::warn!(self.log, "dns relay disabled", err = format!("{err:#}"));
                Vec::new()
            }
        }
    }
}

impl Sandbox for NetnsSandbox {
    fn start(&self, app_log: Option<&File>) -> Result<()> {
        let relays = self.start_dns_relays();

        // The dialer creates the namespace, so everything else comes after it.
        let dialer = Arc::new(start_dialer(&self.log, app_log, &relays).context("dialer helper")?);

        let net_helper = match start_net_helper(&self.cfg, dialer.pid(), app_log, &self.log) {
            Ok(helper) => helper,
            Err(err) => {
                dialer.close();
                return Err(err);
            }
        };

        if let Some(helper) = &net_helper {
            let dialer = Arc::clone(&dialer);
            wait_network_ready(
                move || dialer.route_check(),
                self.cfg.net_wait().as_std(),
                helper.name(),
                &self.log,
            );
        }

        *lock(&self.running) = Some(Running {
            dialer,
            net_helper,
            relays,
        });
        Ok(())
    }

    /// The app must run *inside* the namespace, so it is started through our own
    /// `__spawn` command, which joins the namespace and execs the app in place.
    fn app_command(&self, argv: &[String]) -> Vec<String> {
        let pid = lock(&self.running)
            .as_ref()
            .map_or(0, |running| running.dialer.pid());

        let mut prefixed = vec![
            program::self_path().display().to_string(),
            "__spawn".to_string(),
            "--netns-pid".to_string(),
            pid.to_string(),
            "--".to_string(),
        ];
        prefixed.extend(argv.iter().cloned());
        prefixed
    }

    fn dial(&self, app_port: u16) -> Result<TcpStream> {
        // Cloning the dialer keeps the helper (and the namespace) alive even if a
        // concurrent stop clears the state while this connection is being set up.
        let dialer = lock(&self.running)
            .as_ref()
            .map(|running| Arc::clone(&running.dialer))
            .context("the sandbox is not running")?;
        dialer.dial(app_port)
    }

    fn stop(&self) {
        let Some(running) = lock(&self.running).take() else {
            return;
        };

        if let Some(helper) = running.net_helper {
            helper.stop();
        }
        running.dialer.close();
        drop(running.relays);
    }

    fn describe(&self) -> String {
        let outbound = match self.cfg.net_mode() {
            NetMode::None => "no outbound network".to_string(),
            mode => format!("outbound via {mode}"),
        };
        format!(
            "netns (private loopback, app ports {:?}, {outbound})",
            self.cfg.app_ports()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ListenList;

    fn cfg(net: &str) -> ServiceConfig {
        ServiceConfig {
            name: "a".to_string(),
            listen: ListenList(vec!["127.0.0.1:9000".to_string()]),
            command: vec!["app".to_string()],
            isolation: "netns".to_string(),
            net: net.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn describes_the_isolation_mode() {
        let sandbox = NetnsSandbox::new(cfg("none"), Logger::new(false));
        assert!(sandbox.describe().contains("no outbound network"));
        assert!(sandbox.describe().contains("app ports [9000]"));

        let sandbox = NetnsSandbox::new(cfg("pasta"), Logger::new(false));
        assert!(sandbox.describe().contains("outbound via pasta"));
    }

    #[test]
    fn dialling_before_start_is_an_error_not_a_panic() {
        let sandbox = NetnsSandbox::new(cfg("none"), Logger::new(false));
        assert!(sandbox.dial(9000).is_err());
        sandbox.stop(); // idempotent
        sandbox.stop();
    }

    #[test]
    fn the_app_is_started_through_spawn() {
        let sandbox = NetnsSandbox::new(cfg("none"), Logger::new(false));
        let command = sandbox.app_command(&["app".to_string(), "--port".to_string()]);
        assert_eq!(command[1], "__spawn");
        assert!(command.contains(&"app".to_string()));
    }
}
