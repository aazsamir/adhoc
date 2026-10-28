//! Listen addresses: `127.0.0.1:8080`, or `127.0.0.1:9000=8080` to remap.
//!
//! A service can listen on several host ports, and each one can point at a
//! different port inside the app's sandbox. This module owns that parsing and the
//! `{port}` placeholder, which is how the app is told which port to bind.

use std::collections::HashSet;
use std::fmt;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

use super::ServiceConfig;

/// One host address and the app port it is proxied to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenSpec {
    /// e.g. `127.0.0.1:8080`
    pub addr: String,
    /// the port the app listens on inside its sandbox
    pub app_port: u16,
}

/// A list of listen addresses that also accepts a single string, so
/// `listen: 127.0.0.1:80` and `listen: [127.0.0.1:80, 127.0.0.1:81]` both work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListenList(pub Vec<String>);

struct ListenVisitor;

impl<'de> Visitor<'de> for ListenVisitor {
    type Value = ListenList;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a listen address or a list of listen addresses")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<ListenList, E> {
        Ok(ListenList(vec![value.to_string()]))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<ListenList, E> {
        Ok(ListenList(vec![value]))
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<ListenList, A::Error> {
        let mut out = Vec::new();
        while let Some(value) = seq.next_element::<String>()? {
            out.push(value);
        }
        Ok(ListenList(out))
    }

    fn visit_map<A: de::MapAccess<'de>>(self, _map: A) -> Result<ListenList, A::Error> {
        Err(de::Error::custom(
            "listen must be a string or a list of strings",
        ))
    }

    fn visit_bool<E: de::Error>(self, _v: bool) -> Result<ListenList, E> {
        Err(de::Error::custom(
            "listen must be a string or a list of strings",
        ))
    }
}

impl<'de> Deserialize<'de> for ListenList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ListenVisitor)
    }
}

impl ServiceConfig {
    /// Expands `listen` into host-address/app-port pairs, in config order.
    pub fn listen_specs(&self) -> Result<Vec<ListenSpec>> {
        self.listen
            .0
            .iter()
            .map(|raw| self.parse_listen_entry(raw))
            .collect()
    }

    /// Parses `host:port` or `host:port=appport`.
    fn parse_listen_entry(&self, raw: &str) -> Result<ListenSpec> {
        let (addr, explicit_app_port) = match raw.split_once('=') {
            Some((addr, app_port)) => (addr.trim(), Some(app_port)),
            None => (raw.trim(), None),
        };

        let (host, port_text) = split_host_port(addr).with_context(|| format!("listen {raw:?}"))?;
        let port = parse_port(&port_text).with_context(|| format!("listen {raw:?}: bad port"))?;

        let app_port = match explicit_app_port {
            Some(text) => {
                parse_port(text.trim()).with_context(|| format!("listen {raw:?}: bad app port"))?
            }
            None if self.internal_port != 0 => self.internal_port,
            None => port,
        };

        Ok(ListenSpec {
            addr: join_host_port(&host, &port_text),
            app_port,
        })
    }

    /// The distinct app ports, in config order.
    pub fn app_ports(&self) -> Vec<u16> {
        let Ok(specs) = self.listen_specs() else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut ports = Vec::new();
        for spec in specs {
            if seen.insert(spec.app_port) {
                ports.push(spec.app_port);
            }
        }
        ports
    }

    /// The port used for `{port}` expansion and readiness probing: the first
    /// configured app port.
    pub fn primary_app_port(&self) -> u16 {
        self.app_ports()
            .first()
            .copied()
            .unwrap_or(self.internal_port)
    }
}

/// Splits "host:port", including bracketed IPv6 literals, into host and port.
pub fn split_host_port(addr: &str) -> Result<(String, String)> {
    if addr.is_empty() {
        bail!("missing port in address");
    }

    if let Some(rest) = addr.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            bail!("missing ']' in address");
        };
        let host = &rest[..end];
        let after = &rest[end + 1..];
        let Some(port) = after.strip_prefix(':') else {
            bail!("missing port in address");
        };
        if port.is_empty() {
            bail!("missing port in address");
        }
        if port.contains(':') {
            bail!("too many colons in address");
        }
        return Ok((host.to_string(), port.to_string()));
    }

    let Some((host, port)) = addr.rsplit_once(':') else {
        bail!("missing port in address");
    };
    if host.contains(':') {
        bail!("too many colons in address");
    }
    if port.is_empty() {
        bail!("missing port in address");
    }
    Ok((host.to_string(), port.to_string()))
}

pub fn join_host_port(host: &str, port: &str) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub fn parse_port(text: &str) -> Result<u16> {
    match text.parse::<u32>() {
        Ok(port) if (1..=65535).contains(&port) => Ok(port as u16),
        _ => bail!("not a TCP port: {text:?}"),
    }
}

/// Replaces the `{port}` placeholder in command arguments and env values.
pub fn expand(text: &str, port: u16) -> String {
    text.replace("{port}", &port.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(listen: &[&str], internal_port: u16) -> ServiceConfig {
        ServiceConfig {
            name: "a".to_string(),
            listen: ListenList(listen.iter().map(|item| item.to_string()).collect()),
            internal_port,
            command: vec!["x".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn parses_the_documented_forms() {
        let ok = |listen: &[&str], internal_port: u16, want: &[(&str, u16)]| {
            let specs = service(listen, internal_port).listen_specs().unwrap();
            let want: Vec<ListenSpec> = want
                .iter()
                .map(|(addr, app_port)| ListenSpec {
                    addr: addr.to_string(),
                    app_port: *app_port,
                })
                .collect();
            assert_eq!(specs, want);
        };

        ok(&["127.0.0.1:8080"], 0, &[("127.0.0.1:8080", 8080)]);
        ok(&["0.0.0.0:80"], 0, &[("0.0.0.0:80", 80)]);
        ok(&["127.0.0.1:9000=8080"], 0, &[("127.0.0.1:9000", 8080)]);
        ok(&[" 127.0.0.1:9000 = 8080 "], 0, &[("127.0.0.1:9000", 8080)]);
        ok(
            &["127.0.0.1:9000", "127.0.0.1:9001=8000"],
            8080,
            &[("127.0.0.1:9000", 8080), ("127.0.0.1:9001", 8000)],
        );

        let err = |listen: &str, needle: &str| {
            let err = service(&[listen], 0).listen_specs().unwrap_err();
            assert!(format!("{err:#}").contains(needle), "{listen}: {err:#}");
        };
        err("127.0.0.1", "missing port");
        err("127.0.0.1:http", "bad port");
        err("127.0.0.1:0", "bad port");
        err("127.0.0.1:70000", "bad port");
        err("127.0.0.1:9000=abc", "bad app port");
        err("127.0.0.1:9000=0", "bad app port");

        assert_eq!(
            service(&["[::1]:9000"], 0).listen_specs().unwrap()[0].addr,
            "[::1]:9000"
        );
    }

    #[test]
    fn app_ports_are_deduplicated_in_order() {
        let cfg = service(
            &[
                "127.0.0.1:9000=8080",
                "127.0.0.1:9001=8080",
                "127.0.0.1:9002=9090",
            ],
            0,
        );
        assert_eq!(cfg.app_ports(), vec![8080, 9090]);
        assert_eq!(cfg.primary_app_port(), 8080);
    }

    #[test]
    fn the_port_placeholder_is_expanded() {
        assert_eq!(expand("--port={port}", 8080), "--port=8080");
    }

    #[test]
    fn yaml_accepts_a_string_or_a_list() {
        #[derive(serde::Deserialize, Debug)]
        struct Holder {
            listen: ListenList,
        }

        let parsed: Holder = serde_saphyr::from_str("listen: 127.0.0.1:8080").unwrap();
        assert_eq!(
            parsed.listen,
            ListenList(vec!["127.0.0.1:8080".to_string()])
        );

        let parsed: Holder = serde_saphyr::from_str("listen: [a:1, b:2]").unwrap();
        assert_eq!(
            parsed.listen,
            ListenList(vec!["a:1".to_string(), "b:2".to_string()])
        );

        let err = serde_saphyr::from_str::<Holder>("listen: {a: b}").unwrap_err();
        assert!(err.to_string().contains("string or a list"), "{err}");
    }
}
