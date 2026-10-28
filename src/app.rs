//! The `App` owns the host listeners and runs the show.
//!
//! It does four things, and nothing else:
//!
//! 1. binds every configured listen address up front, so a port conflict fails at
//!    boot instead of on the first request ([`bind`]);
//! 2. accepts connections and hands each one to the matching
//!    [`Service`];
//! 3. ticks a timer that asks idle services to stop;
//! 4. joins it all back together when a shutdown signal arrives.
//!
//! An `App` lives for the whole program run; a `Service` is started and stopped
//! many times underneath it.

use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::log::Logger;
use crate::service::Service;
use crate::sys::proc::{install_signal_handlers, shutdown_requested};
use crate::sys::socket::{bind_tcp, poll_readable};

/// How often idle services are looked at.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_millis(250);
/// How long an accept loop waits before checking whether it should stop.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How often the main thread checks for a shutdown signal.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// One host address, bound for a service.
///
/// Reserving all of them before any app runs turns port conflicts into a startup
/// error. `app_port` is the port the app itself listens on inside its sandbox,
/// which is *not* necessarily the port we bound here (see `host:port=appport`).
#[derive(Debug)]
pub struct ReservedListener {
    /// Index into [`Config::services`].
    service: usize,
    addr: String,
    app_port: u16,
    listener: TcpListener,
}

impl ReservedListener {
    /// Builds one from a socket that was bound (or inherited) elsewhere.
    pub fn new(service: usize, addr: String, app_port: u16, listener: TcpListener) -> Self {
        ReservedListener {
            service,
            addr,
            app_port,
            listener,
        }
    }

    /// The service this listener belongs to.
    pub fn service(&self) -> usize {
        self.service
    }

    /// The configured `host:port` this listener was bound to.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// The port the app listens on inside its sandbox.
    pub fn app_port(&self) -> u16 {
        self.app_port
    }

    /// The bound socket, for handing it to the child of a re-exec.
    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }
}

/// Binds every configured listen address.
pub fn bind(cfg: &Config) -> Result<Vec<ReservedListener>> {
    let mut reserved = Vec::new();

    for (index, service) in cfg.services.iter().enumerate() {
        for spec in service.listen_specs()? {
            let listener = bind_tcp(&spec.addr)
                .with_context(|| format!("{}: listen {}", service.name, spec.addr))?;
            reserved.push(ReservedListener {
                service: index,
                addr: spec.addr,
                app_port: spec.app_port,
                listener,
            });
        }
    }
    Ok(reserved)
}

pub struct App {
    services: Vec<Arc<Service>>,
    log: Logger,
}

impl App {
    pub fn new(cfg: &Config, log: &Logger) -> Result<App> {
        let services = cfg
            .services
            .iter()
            .map(|service| Service::new(service.clone(), &cfg.log_dir, log).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;

        Ok(App {
            services,
            log: log.clone(),
        })
    }

    /// Serves until a shutdown signal arrives.
    pub fn run(&self, listeners: Vec<ReservedListener>) -> Result<()> {
        self.log_listeners(&listeners);

        install_signal_handlers();

        let accept_loops: Vec<_> = listeners
            .into_iter()
            .map(|reserved| {
                let service = Arc::clone(&self.services[reserved.service]);
                let log = self.log.clone();
                thread::spawn(move || accept_loop(service, reserved, log))
            })
            .collect();

        let idle_loop = {
            let services = self.services.clone();
            thread::spawn(move || idle_loop(services))
        };

        while !shutdown_requested() {
            thread::sleep(SHUTDOWN_POLL_INTERVAL);
        }
        self.log.info("shutting down", &[]);

        // The accept loops notice the shutdown within one poll interval. Waiting
        // for them first means no new connection can start an app behind our back
        // while we are stopping everything.
        for handle in accept_loops {
            let _ = handle.join();
        }
        self.stop_all_services();
        let _ = idle_loop.join();
        Ok(())
    }

    fn log_listeners(&self, listeners: &[ReservedListener]) {
        for reserved in listeners {
            let service = &self.services[reserved.service];
            crate::info!(
                self.log,
                "listening",
                service = service.cfg().name,
                addr = reserved.addr,
                app_port = reserved.app_port,
                idle_timeout = service.cfg().idle(),
                isolation = service.cfg().isolation_mode(),
            );
        }
    }

    /// Stops every app in parallel and waits for them.
    fn stop_all_services(&self) {
        thread::scope(|scope| {
            for service in &self.services {
                let service = Arc::clone(service);
                scope.spawn(move || service.shutdown());
            }
        });
    }
}

/// Accepts connections on one host address until the launcher shuts down.
fn accept_loop(service: Arc<Service>, reserved: ReservedListener, log: Logger) {
    let listener = &reserved.listener;
    let app_port = reserved.app_port;

    loop {
        if shutdown_requested() {
            return;
        }

        match poll_readable(listener.as_raw_fd(), ACCEPT_POLL_INTERVAL) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(err) => {
                if !shutdown_requested() {
                    crate::debug!(log, "accept", service = service.cfg().name, err = err);
                }
                return;
            }
        }

        match listener.accept() {
            Ok((client, _peer)) => {
                let _ = client.set_nodelay(true);
                let service = Arc::clone(&service);
                thread::spawn(move || service.handle(client, app_port));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) => {
                if !shutdown_requested() {
                    crate::debug!(log, "accept", service = service.cfg().name, err = err);
                }
                return;
            }
        }
    }
}

/// Periodically stops services that have been quiet for longer than their
/// `idle_timeout`.
fn idle_loop(services: Vec<Arc<Service>>) {
    loop {
        if shutdown_requested() {
            return;
        }
        thread::sleep(IDLE_CHECK_INTERVAL);
        for service in &services {
            service.stop_if_idle();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Duration as Timeout, ListenList, ServiceConfig};

    fn free_port() -> u16 {
        bind_tcp("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn service_cfg(name: &str, listen: &[&str]) -> ServiceConfig {
        ServiceConfig {
            name: name.to_string(),
            listen: ListenList(listen.iter().map(|item| item.to_string()).collect()),
            command: vec!["sleep".to_string(), "5".to_string()],
            isolation: "port".to_string(),
            internal_port: 1,
            log: "discard".to_string(),
            idle_timeout: Some(Timeout::from_millis(100)),
            ..Default::default()
        }
    }

    fn config(services: Vec<ServiceConfig>) -> Config {
        Config {
            log_dir: String::new(),
            services,
        }
    }

    #[test]
    fn binding_reserves_every_listen_address() {
        let (one, two, three) = (free_port(), free_port(), free_port());
        let a = format!("127.0.0.1:{one}");
        let b = format!("127.0.0.1:{two}");
        let c = format!("127.0.0.1:{three}");

        let reserved = bind(&config(vec![
            service_cfg("a", &[&a]),
            service_cfg("b", &[&b, &c]),
        ]))
        .unwrap();

        assert_eq!(reserved.len(), 3);
        assert_eq!(reserved[0].service(), 0);
        assert_eq!(reserved[1].service(), 1);
        assert_eq!(reserved[2].service(), 1);
        assert_eq!(reserved[0].addr(), a);
        assert!(reserved[2].listener().local_addr().is_ok());
    }

    #[test]
    fn binding_fails_when_a_port_is_taken() {
        let taken = bind_tcp("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap().to_string();

        let err = bind(&config(vec![service_cfg("a", &[&addr])])).unwrap_err();
        assert!(format!("{err:#}").contains("listen"), "{err:#}");
    }

    #[test]
    fn app_construction_opens_the_configured_log_files() {
        let dir = std::env::temp_dir().join(format!("adhoc-app-{}", free_port()));
        let _ = std::fs::remove_dir_all(&dir);

        let listen = format!("127.0.0.1:{}", free_port());
        let mut service = service_cfg("web", &[&listen]);
        service.log = "file".to_string();
        let cfg = Config {
            log_dir: dir.display().to_string(),
            services: vec![service],
        };

        let app = App::new(&cfg, &Logger::new(false)).unwrap();
        assert_eq!(app.services.len(), 1);
        assert!(dir.join("web.log").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
