//! What keeps the app's listening socket away from the proxy's.
//!
//! There are exactly two strategies, and picking one is the whole point of the
//! `isolation:` config key:
//!
//! | | [`PortSandbox`] (`isolation: port`) | [`NetnsSandbox`] (`isolation: netns`) |
//! |---|---|---|
//! | app keeps its hardcoded port | no | **yes** |
//! | needs privileges | no | no (unprivileged user namespaces) |
//! | outbound network | yes, the host's | only via `net: pasta` / `slirp4netns` |
//! | per connection | nothing | one fd passed from the helper |
//!
//! [`Service`](crate::service::Service) only ever talks to the [`Sandbox`] trait,
//! so the interesting lifecycle code contains no namespace details at all.

mod netns;
mod port;

pub use netns::NetnsSandbox;
pub use port::PortSandbox;

use std::fs::File;
use std::net::TcpStream;

use anyhow::Result;

use crate::config::{Isolation, ServiceConfig};
use crate::log::Logger;

/// How an app is kept apart from the proxy, and how the proxy reaches it.
pub trait Sandbox: Send + Sync {
    /// Prepares the sandbox. Called before the app is spawned.
    fn start(&self, app_log: Option<&File>) -> Result<()>;

    /// Wraps the app's argv so it runs inside the sandbox.
    fn app_command(&self, argv: &[String]) -> Vec<String>;

    /// Connects to the app's port from the manager's side.
    fn dial(&self, app_port: u16) -> Result<TcpStream>;

    /// Tears the sandbox down. Safe to call more than once, and before `start`.
    fn stop(&self);

    /// A one-line summary for the log.
    fn describe(&self) -> String;
}

/// Builds the sandbox the config asks for.
pub fn new_sandbox(cfg: &ServiceConfig, log: &Logger) -> Result<Box<dyn Sandbox>> {
    Ok(match cfg.isolation_mode() {
        Isolation::Port => Box::new(PortSandbox::new(cfg.clone())),
        Isolation::Netns => Box::new(NetnsSandbox::new(cfg.clone(), log.clone())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ListenList;

    fn cfg(isolation: &str, internal_port: u16) -> ServiceConfig {
        ServiceConfig {
            name: "a".to_string(),
            listen: ListenList(vec!["127.0.0.1:9000".to_string()]),
            internal_port,
            command: vec!["app".to_string()],
            isolation: isolation.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_factory_picks_the_right_driver() {
        let log = Logger::new(false);
        let sandbox = new_sandbox(&cfg("port", 9001), &log).unwrap();
        assert!(sandbox.describe().starts_with("port"));

        let sandbox = new_sandbox(&cfg("netns", 9000), &log).unwrap();
        assert!(sandbox.describe().starts_with("netns"));
    }
}
