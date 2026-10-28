//! The YAML configuration, and the small pieces the rest of the program works
//! with: addresses to listen on, ports the app uses, timeouts and modes.
//!
//! The file is organised as a document: [`Config`] holds [`ServiceConfig`]s, and
//! validation is all in one place at the bottom so the rules read as a list.
//!
//! Two pieces have their own module because they carry real logic:
//!
//! * [`duration`] — the `30s` / `1h30m` / `30` YAML shape
//! * [`listen`] — `host:port=appport` and the `{port}` placeholder

pub mod duration;
pub mod listen;

pub use duration::Duration;
pub use listen::{ListenList, ListenSpec, expand};

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

// Defaults for every optional timeout. Named and kept together so the accessors
// below read like a table.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_NET_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// private network namespace (default)
    Netns,
    /// no namespace, the app is told to use another port
    Port,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    /// app has loopback only
    None,
    Pasta,
    Slirp4netns,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsMode {
    /// relay the host's resolvers into the namespace
    Auto,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    /// `log_dir/<service>.log`
    File,
    Inherit,
    Discard,
}

/// The whole YAML document.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub log_dir: String,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

/// One lazy-started app.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    #[serde(default)]
    pub name: String,

    /// One host address or a list of them:
    ///
    /// ```yaml
    /// listen: 127.0.0.1:8080
    /// listen: ["0.0.0.0:8889", "0.0.0.0:8554"]
    /// listen: ["127.0.0.1:9000=8080"]   # host 9000 -> app 8080
    /// ```
    ///
    /// Without "=appport" the app port defaults to the listen port (or to
    /// `internal_port` when that is set).
    #[serde(default)]
    pub listen: ListenList,

    /// The app's port when the listen entry does not spell it out.
    #[serde(default)]
    pub internal_port: u16,

    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub workdir: String,
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// How the app's listening socket is kept apart from the proxy's.
    #[serde(default)]
    pub isolation: String,

    /// Outbound network access for a netns service.
    #[serde(default)]
    pub net: String,
    #[serde(default)]
    pub net_args: Vec<String>,
    /// Exact argv when `net` is "custom".
    #[serde(default)]
    pub net_command: Vec<String>,

    /// Whether the host's resolvers are relayed into the namespace.
    #[serde(default)]
    pub dns: String,
    #[serde(default)]
    pub dns_servers: Vec<String>,

    // `None` means "use the default", so an explicit `0` can mean "never"
    // (idle_timeout) instead of "use the default".
    #[serde(default)]
    pub idle_timeout: Option<Duration>,
    #[serde(default)]
    pub startup_timeout: Option<Duration>,
    #[serde(default)]
    pub stop_timeout: Option<Duration>,
    #[serde(default)]
    pub connect_timeout: Option<Duration>,
    #[serde(default)]
    pub net_timeout: Option<Duration>,

    #[serde(default)]
    pub log: String,
}

// --- defaulted values -------------------------------------------------------

impl ServiceConfig {
    pub fn idle(&self) -> Duration {
        self.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT)
    }

    pub fn startup(&self) -> Duration {
        self.startup_timeout.unwrap_or(DEFAULT_STARTUP_TIMEOUT)
    }

    pub fn stop(&self) -> Duration {
        self.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT)
    }

    pub fn connect(&self) -> Duration {
        self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT)
    }

    pub fn net_wait(&self) -> Duration {
        self.net_timeout.unwrap_or(DEFAULT_NET_TIMEOUT)
    }

    pub fn log_mode(&self) -> LogMode {
        match self.log.as_str() {
            "inherit" => LogMode::Inherit,
            "discard" => LogMode::Discard,
            _ => LogMode::File,
        }
    }

    pub fn net_mode(&self) -> NetMode {
        match self.net.as_str() {
            "pasta" => NetMode::Pasta,
            "slirp4netns" => NetMode::Slirp4netns,
            "custom" => NetMode::Custom,
            _ => NetMode::None,
        }
    }

    pub fn dns_mode(&self) -> DnsMode {
        match self.dns.as_str() {
            "off" => DnsMode::Off,
            _ => DnsMode::Auto,
        }
    }

    pub fn isolation_mode(&self) -> Isolation {
        match self.isolation.as_str() {
            "port" => Isolation::Port,
            _ => Isolation::Netns,
        }
    }
}

impl std::fmt::Display for Isolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Isolation::Netns => "netns",
            Isolation::Port => "port",
        })
    }
}

impl std::fmt::Display for NetMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NetMode::None => "none",
            NetMode::Pasta => "pasta",
            NetMode::Slirp4netns => "slirp4netns",
            NetMode::Custom => "custom",
        })
    }
}

impl std::fmt::Display for DnsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DnsMode::Auto => "auto",
            DnsMode::Off => "off",
        })
    }
}

impl std::fmt::Display for LogMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LogMode::File => "file",
            LogMode::Inherit => "inherit",
            LogMode::Discard => "discard",
        })
    }
}

// --- loading and validation -------------------------------------------------

impl Config {
    /// Parses and validates a YAML config file.
    pub fn load(path: &Path) -> Result<Config> {
        let data =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Config =
            serde_saphyr::from_str(&data).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Checks the whole document and reports the first problem.
    pub fn validate(&self) -> Result<()> {
        if self.services.is_empty() {
            bail!("no services configured");
        }

        // Wildcard and loopback binds of the same port collide on purpose: two
        // services cannot own the same host port.
        let mut names: HashSet<&str> = HashSet::new();
        let mut listen_ports: HashMap<String, &str> = HashMap::new();

        for (index, service) in self.services.iter().enumerate() {
            if service.name.is_empty() {
                bail!("service #{}: name is required", index + 1);
            }
            if !names.insert(service.name.as_str()) {
                bail!("duplicate service name {:?}", service.name);
            }

            let specs = service.listen_specs()?;
            service.validate(&specs)?;

            for spec in &specs {
                let port = listen::split_host_port(&spec.addr)
                    .map(|(_, port)| port)
                    .unwrap_or_default();
                if let Some(owner) = listen_ports.insert(port.clone(), &service.name) {
                    bail!(
                        "{}: listen port {port} already used by {owner}",
                        service.name
                    );
                }
            }
        }
        Ok(())
    }
}

impl ServiceConfig {
    fn validate(&self, specs: &[ListenSpec]) -> Result<()> {
        if self.listen.0.is_empty() {
            bail!("{}: listen is required", self.name);
        }
        if self.command.is_empty() {
            bail!("{}: command is required", self.name);
        }

        self.validate_isolation(specs)?;
        self.validate_net()?;
        self.validate_dns()?;
        self.validate_log()
    }

    fn validate_isolation(&self, specs: &[ListenSpec]) -> Result<()> {
        match self.isolation.as_str() {
            "" | "netns" => Ok(()),

            "port" => {
                // Without a namespace the app and the proxy share one network
                // stack, so the app needs a different port and exactly one can
                // be templated into the command line.
                if self.app_ports().len() != 1 {
                    bail!(
                        "{}: isolation=port supports a single app port (use netns)",
                        self.name
                    );
                }
                for spec in specs {
                    let (_, listen_port) =
                        listen::split_host_port(&spec.addr).with_context(|| self.name.clone())?;
                    if listen_port == spec.app_port.to_string() {
                        bail!(
                            "{}: isolation=port needs the app port ({}) to differ from every listen port",
                            self.name,
                            spec.app_port
                        );
                    }
                }
                Ok(())
            }

            other => bail!("{}: unknown isolation {other:?} (netns|port)", self.name),
        }
    }

    fn validate_net(&self) -> Result<()> {
        match self.net.as_str() {
            "" | "none" | "pasta" | "slirp4netns" => {}
            "custom" => {
                if self.net_command.is_empty() {
                    bail!("{}: net: custom requires net_command", self.name);
                }
            }
            other => bail!(
                "{}: unknown net {other:?} (none|pasta|slirp4netns|custom)",
                self.name
            ),
        }

        if self.net_mode() != NetMode::None && self.isolation_mode() != Isolation::Netns {
            bail!(
                "{}: net: {} only makes sense with isolation: netns",
                self.name,
                self.net_mode()
            );
        }
        Ok(())
    }

    fn validate_dns(&self) -> Result<()> {
        match self.dns.as_str() {
            "" | "auto" | "off" => {}
            other => bail!("{}: unknown dns {other:?} (auto|off)", self.name),
        }
        for server in &self.dns_servers {
            if server.parse::<IpAddr>().is_err() {
                bail!(
                    "{}: dns_servers: {server:?} is not an IP address",
                    self.name
                );
            }
        }
        Ok(())
    }

    fn validate_log(&self) -> Result<()> {
        match self.log.as_str() {
            "" | "file" | "inherit" | "discard" => Ok(()),
            other => bail!(
                "{}: unknown log mode {other:?} (file|inherit|discard)",
                self.name
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, listen: &[&str]) -> ServiceConfig {
        ServiceConfig {
            name: name.to_string(),
            listen: ListenList(listen.iter().map(|l| l.to_string()).collect()),
            command: vec!["x".to_string()],
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
    fn unset_fields_use_the_documented_defaults() {
        let s = ServiceConfig::default();
        assert_eq!(s.idle(), DEFAULT_IDLE_TIMEOUT);
        assert_eq!(s.startup(), DEFAULT_STARTUP_TIMEOUT);
        assert_eq!(s.stop(), DEFAULT_STOP_TIMEOUT);
        assert_eq!(s.connect(), DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(s.net_wait(), DEFAULT_NET_TIMEOUT);
        assert_eq!(s.net_mode(), NetMode::None);
        assert_eq!(s.dns_mode(), DnsMode::Auto);
        assert_eq!(s.log_mode(), LogMode::File);
        assert_eq!(s.isolation_mode(), Isolation::Netns);
    }

    #[test]
    fn set_timeouts_win_over_the_defaults() {
        let s = ServiceConfig {
            idle_timeout: Some(Duration::from_secs(1)),
            startup_timeout: Some(Duration::from_secs(2)),
            stop_timeout: Some(Duration::from_secs(3)),
            connect_timeout: Some(Duration::from_secs(4)),
            net_timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        assert_eq!(s.idle().nanos(), 1_000_000_000);
        assert_eq!(s.startup().nanos(), 2_000_000_000);
        assert_eq!(s.stop().nanos(), 3_000_000_000);
        assert_eq!(s.connect().nanos(), 4_000_000_000);
        assert_eq!(s.net_wait().nanos(), 5_000_000_000);
    }

    #[test]
    fn modes_render_the_way_the_config_spells_them() {
        assert_eq!(Isolation::Netns.to_string(), "netns");
        assert_eq!(Isolation::Port.to_string(), "port");
        assert_eq!(NetMode::Slirp4netns.to_string(), "slirp4netns");
        assert_eq!(DnsMode::Off.to_string(), "off");
        assert_eq!(LogMode::Discard.to_string(), "discard");
    }

    #[test]
    fn the_document_must_have_services_with_unique_names() {
        assert!(Config::default().validate().is_err(), "no services");

        let duplicate_names = config(vec![
            svc("a", &["127.0.0.1:80"]),
            svc("a", &["127.0.0.1:81"]),
        ]);
        assert!(duplicate_names.validate().is_err());

        let duplicate_port = config(vec![svc("a", &["127.0.0.1:80"]), svc("b", &["0.0.0.0:80"])]);
        assert!(duplicate_port.validate().is_err());
    }

    #[test]
    fn every_field_is_validated() {
        let cases: Vec<Config> = vec![
            config(vec![ServiceConfig {
                isolation: "chroot".to_string(),
                ..svc("a", &["127.0.0.1:80"])
            }]),
            config(vec![ServiceConfig {
                name: String::new(),
                ..svc("a", &["127.0.0.1:80"])
            }]),
            config(vec![ServiceConfig {
                listen: ListenList::default(),
                ..svc("a", &["127.0.0.1:80"])
            }]),
            config(vec![ServiceConfig {
                command: Vec::new(),
                ..svc("a", &["127.0.0.1:80"])
            }]),
            config(vec![ServiceConfig {
                net: "tun2socks".to_string(),
                ..svc("a", &["127.0.0.1:80"])
            }]),
            config(vec![ServiceConfig {
                net: "custom".to_string(),
                ..svc("a", &["127.0.0.1:80"])
            }]),
            config(vec![ServiceConfig {
                dns: "sometimes".to_string(),
                ..svc("a", &["127.0.0.1:80"])
            }]),
            config(vec![ServiceConfig {
                dns_servers: vec!["not-an-ip".to_string()],
                ..svc("a", &["127.0.0.1:80"])
            }]),
            config(vec![ServiceConfig {
                log: "banana".to_string(),
                ..svc("a", &["127.0.0.1:80"])
            }]),
        ];

        for (index, cfg) in cases.iter().enumerate() {
            assert!(cfg.validate().is_err(), "case {index} should be rejected");
        }
    }

    #[test]
    fn reasonable_configs_are_accepted() {
        for cfg in [
            config(vec![svc("a", &["127.0.0.1:80=80"])]),
            config(vec![svc("a", &["127.0.0.1:80", "127.0.0.1:81=9000"])]),
            config(vec![ServiceConfig {
                isolation: "port".to_string(),
                internal_port: 8080,
                ..svc("a", &["127.0.0.1:9000"])
            }]),
        ] {
            cfg.validate().unwrap();
        }
    }

    #[test]
    fn net_needs_the_netns_isolation() {
        let cfg = config(vec![ServiceConfig {
            net: "pasta".to_string(),
            isolation: "port".to_string(),
            internal_port: 81,
            ..svc("a", &["127.0.0.1:80"])
        }]);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn isolation_port_needs_a_single_app_port() {
        let cfg = config(vec![ServiceConfig {
            isolation: "port".to_string(),
            listen: ListenList(vec![
                "127.0.0.1:80=8080".to_string(),
                "127.0.0.1:81=8081".to_string(),
            ]),
            ..svc("a", &["127.0.0.1:80"])
        }]);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn typos_are_rejected_instead_of_ignored() {
        let err =
            serde_saphyr::from_str::<Config>("services:\n  - name: a\n    lisen: 127.0.0.1:80\n")
                .unwrap_err();
        assert!(err.to_string().contains("lisen"), "{err}");
    }

    #[test]
    fn loading_a_missing_file_reports_the_path() {
        let err = Config::load(Path::new("/definitely/not/here.yaml")).unwrap_err();
        assert!(
            format!("{err:#}").contains("/definitely/not/here.yaml"),
            "{err:#}"
        );
    }
}
