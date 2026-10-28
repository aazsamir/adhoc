//! `isolation: port` — no sandbox at all.
//!
//! The app is simply told to bind a different port through the `{port}`
//! placeholder in its command line, and the proxy dials it on the host's own
//! loopback.
//!
//! This works for apps that accept a port argument or a `PORT` environment
//! variable, and needs no privileges — but the app keeps the host network: it
//! can reach anything the launcher can, and it cannot keep a hardcoded port.

use std::fs::File;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};

use anyhow::{Context, Result};

use crate::config::ServiceConfig;
use crate::sandbox::Sandbox;

pub struct PortSandbox {
    cfg: ServiceConfig,
}

impl PortSandbox {
    pub fn new(cfg: ServiceConfig) -> PortSandbox {
        PortSandbox { cfg }
    }
}

impl Sandbox for PortSandbox {
    fn start(&self, _app_log: Option<&File>) -> Result<()> {
        Ok(())
    }

    fn app_command(&self, argv: &[String]) -> Vec<String> {
        argv.to_vec()
    }

    fn dial(&self, app_port: u16) -> Result<TcpStream> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, app_port));
        TcpStream::connect_timeout(&address, self.cfg.connect().as_std())
            .with_context(|| format!("connect to the app on 127.0.0.1:{app_port}"))
    }

    fn stop(&self) {}

    fn describe(&self) -> String {
        format!(
            "port (no isolation, app port {})",
            self.cfg.primary_app_port()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Duration as Timeout, ListenList, ServiceConfig};

    fn cfg(connect_timeout_ms: Option<i64>) -> ServiceConfig {
        let mut config = ServiceConfig {
            name: "a".to_string(),
            listen: ListenList(vec!["127.0.0.1:9000".to_string()]),
            internal_port: 9001,
            command: vec!["app".to_string()],
            isolation: "port".to_string(),
            ..Default::default()
        };
        config.connect_timeout = connect_timeout_ms.map(Timeout::from_millis);
        config
    }

    #[test]
    fn describes_itself() {
        let sandbox = PortSandbox::new(cfg(None));
        assert_eq!(sandbox.describe(), "port (no isolation, app port 9001)");
        let _ = Config::default();
    }

    #[test]
    fn dials_a_real_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sandbox = PortSandbox::new(cfg(None));

        let connection = sandbox.dial(port).expect("dial");
        assert!(listener.accept().is_ok());
        drop(connection);
    }

    #[test]
    fn a_closed_port_is_an_error_not_a_hang() {
        let sandbox = PortSandbox::new(cfg(Some(200)));
        let started = std::time::Instant::now();
        let err = sandbox.dial(1).unwrap_err();
        assert!(format!("{err:#}").contains("127.0.0.1:1"), "{err:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
