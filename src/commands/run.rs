//! The `run` and `__serve` commands.
//!
//! `run` is what users start: load the config, bind every port, and then either
//! serve directly (root) or hand over to [`reexec`], which re-executes us inside
//! a user namespace and comes back as `__serve`.
//!
//! Both commands end up in `serve`, which is the only place that knows about
//! [`App`].

use std::path::Path;

use anyhow::{Context, Result};

use crate::app::{self, App, ReservedListener};
use crate::cli::Args;
use crate::commands::reexec;
use crate::config::{Config, Isolation, NetMode};
use crate::log::Logger;
use crate::netns::nethelper::net_argv;
use crate::program;
use crate::sys::proc::euid;

pub const DEFAULT_CONFIG: &str = "adhoc.yaml";

/// `adhoc run`: start the launcher.
pub fn cmd_run(args: &[String]) -> i32 {
    let parsed = Args::parse(args, &["config"]);
    let log = Logger::new(parsed.has("debug"));

    let config_path = parsed.value_or("config", DEFAULT_CONFIG);
    let cfg = match load_config(config_path, &log) {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };

    // Reserve the ports in the host namespace before doing anything else, so a
    // conflict fails at startup rather than on the first request.
    let listeners = match app::bind(&cfg) {
        Ok(listeners) => listeners,
        Err(err) => {
            log.error("bind", &[("err", format!("{err:#}"))]);
            return 1;
        }
    };

    // Root needs no user namespace; everyone else borrows `unshare --user`.
    if euid() != 0 && uses_namespaces(&cfg) {
        return match reexec::restart(listeners, config_path, parsed.has("debug"), &log)
            .context("unprivileged user namespaces may be disabled; try `isolation: port`")
        {
            Ok(code) => code,
            Err(err) => {
                log.error("user namespace", &[("err", format!("{err:#}"))]);
                1
            }
        };
    }

    match serve(&cfg, listeners, &log) {
        Ok(()) => 0,
        Err(err) => {
            log.error("run", &[("err", format!("{err:#}"))]);
            1
        }
    }
}

/// `adhoc __serve`: the launcher itself, either as root or inside the user
/// namespace prepared by [`reexec`].
pub fn cmd_serve(args: &[String]) -> i32 {
    let parsed = Args::parse(args, &["config", "map"]);
    let log = Logger::new(parsed.has("debug"));

    match serve_from_args(&parsed, &log) {
        Ok(()) => 0,
        Err(err) => {
            log.error("serve", &[("err", format!("{err:#}"))]);
            1
        }
    }
}

fn serve_from_args(parsed: &Args, log: &Logger) -> Result<()> {
    let cfg = Config::load(Path::new(parsed.value_or("config", DEFAULT_CONFIG)))?;

    let listeners = match parsed.value("map") {
        // Inherited from `run` through the re-exec (the normal path).
        Some(spec) if !spec.is_empty() => reexec::listeners_from_fds(spec)?,
        // Invoked by hand: bind here, exactly like `run` would.
        _ => app::bind(&cfg)?,
    };
    serve(&cfg, listeners, log)
}

/// The last step shared by `run` and `__serve`.
fn serve(cfg: &Config, listeners: Vec<ReservedListener>, log: &Logger) -> Result<()> {
    preflight(cfg)?;
    App::new(cfg, log)?.run(listeners)
}

/// Loads and validates the config, reporting a fatal error the way a CLI should.
fn load_config(path: &str, log: &Logger) -> Result<Config, i32> {
    Config::load(Path::new(path)).map_err(|err| {
        log.error("config", &[("err", format!("{err:#}"))]);
        1
    })
}

/// Whether any service needs a network namespace at all.
pub fn uses_namespaces(cfg: &Config) -> bool {
    cfg.services
        .iter()
        .any(|service| service.isolation_mode() == Isolation::Netns)
}

/// Fails fast on things that would otherwise only show up as a failed first
/// request.
pub fn preflight(cfg: &Config) -> Result<()> {
    for service in &cfg.services {
        if service.net_mode() == NetMode::None || service.isolation_mode() != Isolation::Netns {
            continue;
        }
        let argv = net_argv(service, "/proc/self/ns/net")?;
        if !program::exists(&argv[0]) {
            anyhow::bail!(
                "{}: net: {} needs {:?}: install it (e.g. pacman -S passt) or set net: none",
                service.name,
                service.net,
                argv[0]
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ListenList, ServiceConfig};

    fn cfg(services: Vec<ServiceConfig>) -> Config {
        Config {
            log_dir: String::new(),
            services,
        }
    }

    fn service(name: &str, isolation: &str, net: &str) -> ServiceConfig {
        ServiceConfig {
            name: name.to_string(),
            listen: ListenList(vec!["127.0.0.1:9000".to_string()]),
            command: vec!["true".to_string()],
            isolation: isolation.to_string(),
            net: net.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn detects_services_that_need_namespaces() {
        assert!(!uses_namespaces(&cfg(vec![service("a", "port", "none")])));
        assert!(uses_namespaces(&cfg(vec![service("a", "netns", "none")])));
    }

    #[test]
    fn preflight_checks_the_net_helper_binary() {
        let mut custom = service("a", "netns", "custom");
        custom.net_command = vec!["definitely-not-a-net-helper".to_string()];
        let err = preflight(&cfg(vec![custom])).unwrap_err();
        assert!(format!("{err:#}").contains("needs"), "{err:#}");

        // net: none never needs a helper, nor does isolation: port.
        preflight(&cfg(vec![service("a", "netns", "none")])).unwrap();
        preflight(&cfg(vec![service("a", "port", "none")])).unwrap();
    }
}
