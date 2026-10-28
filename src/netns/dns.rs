//! DNS inside a private network namespace.
//!
//! The app runs in a fresh network namespace but keeps reading the host's
//! /etc/resolv.conf (we share the mount namespace and do not rewrite files).
//! That breaks in two common cases:
//!
//! ```text
//! host resolver: 127.0.0.53 (systemd-resolved) or 127.0.0.1 (dnsmasq)
//!   -> inside the namespace nothing listens on that loopback address
//! host resolver: a LAN address
//!   -> the namespace has no route to it
//! ```
//!
//! Both are fixed the same way, without touching any file:
//!
//! 1. inside the namespace, make the resolver address local to `lo`
//! 2. listen on `<resolver>:53` there
//! 3. forward every raw UDP datagram through a socket that lives in the host
//!    namespace (which can reach the real resolver)
//!
//! The host-side sockets are opened by the manager before the helper starts and
//! are inherited by the dialer helper. The dialer then runs `serve_dns_relay`
//! inside the namespace.
//!
//! Caveat: the resolver's address becomes local to the app. That only matters
//! if the resolver is also some other service the app connects to.

use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::RawFd;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{DnsMode, ServiceConfig};
use crate::log::Logger;
use crate::sys::netlink::add_addr;

const DNS_PORT: u16 = 53;
const DNS_QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const DNS_SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
const DNS_BUFFER_SIZE: usize = 8192;

/// One resolver. It exists in two halves: the socket in the host namespace
/// (opened by the manager, inherited by the dialer helper) and the address the
/// app sees inside the namespace.
pub struct DnsRelay {
    pub resolver: Ipv4Addr,
    pub socket: UdpSocket,
}

/// Reads IPv4 nameserver entries from a resolv.conf style file. IPv6 resolvers
/// are skipped: the relay speaks udp4.
pub fn resolver_addrs(path: &Path) -> Vec<Ipv4Addr> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut resolvers = Vec::new();
    for raw in content.lines() {
        let line = match raw.find(['#', ';']) {
            Some(index) => &raw[..index],
            None => raw,
        };
        let mut fields = line.split_whitespace();
        let (Some(keyword), Some(address)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("nameserver") {
            continue;
        }
        let Ok(address) = address.parse::<Ipv4Addr>() else {
            continue;
        };
        if address.is_unspecified() || resolvers.contains(&address) {
            continue;
        }
        resolvers.push(address);
    }
    resolvers
}

/// Opens one host-namespace socket per resolver, ready to be inherited.
pub fn prepare_dns_relays(resolvers: &[Ipv4Addr]) -> Result<Vec<DnsRelay>> {
    resolvers.iter().map(|ip| open_dns_relay(*ip)).collect()
}

fn open_dns_relay(resolver: Ipv4Addr) -> Result<DnsRelay> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect((resolver, DNS_PORT))?;
    socket.set_read_timeout(Some(DNS_SOCKET_TIMEOUT))?;
    socket.set_write_timeout(Some(DNS_SOCKET_TIMEOUT))?;
    Ok(DnsRelay { resolver, socket })
}

/// Adopts an inherited host-side socket in the dialer helper.
pub fn relay_from_fd(fd: RawFd) -> Result<UdpSocket> {
    Ok(crate::sys::fd::adopt_udp_socket(fd)?)
}

/// Decides which resolvers to relay for a service.
pub fn resolve_dns_targets(cfg: &ServiceConfig, log: &Logger) -> Vec<Ipv4Addr> {
    match cfg.dns_mode() {
        DnsMode::Off => Vec::new(),

        DnsMode::Auto => {
            if !cfg.dns_servers.is_empty() {
                return cfg
                    .dns_servers
                    .iter()
                    .filter_map(|server| server.parse::<Ipv4Addr>().ok())
                    .collect();
            }

            let path = std::env::var("ADHOC_RESOLV_CONF")
                .unwrap_or_else(|_| "/etc/resolv.conf".to_string());
            let resolvers = resolver_addrs(Path::new(&path));
            if resolvers.is_empty() {
                crate::debug!(log, "dns: no usable nameservers found", path = path);
            }
            resolvers
        }
    }
}

pub fn format_ips(ips: &[Ipv4Addr]) -> String {
    ips.iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

// --- inside the namespace ---------------------------------------------------

/// Runs in the app's namespace: listens on `<resolver>:53` and forwards every
/// query over the inherited host-side socket.
pub fn serve_dns_relay(host: UdpSocket, resolver: Ipv4Addr, log: Logger) -> Result<()> {
    let local = bind_resolver_inside_namespace(resolver, &log)?;
    let _ = host.set_read_timeout(Some(DNS_QUERY_TIMEOUT));
    let _ = host.set_write_timeout(Some(DNS_QUERY_TIMEOUT));

    std::thread::spawn(move || relay_queries(local, host, resolver, log));
    Ok(())
}

/// Makes the resolver address local to `lo` (unless it already is) and binds
/// `<resolver>:53`.
fn bind_resolver_inside_namespace(resolver: Ipv4Addr, log: &Logger) -> Result<UdpSocket> {
    if !resolver.is_loopback() {
        // Additive: 127.0.0.1/8 stays intact.
        if let Err(err) = add_addr("lo", resolver, 32) {
            crate::debug!(
                log,
                "dns: cannot add address to lo",
                addr = resolver,
                err = err
            );
        }
    }

    UdpSocket::bind((resolver, DNS_PORT))
        .with_context(|| format!("bind {resolver}:{DNS_PORT} inside the namespace"))
}

fn relay_queries(local: UdpSocket, host: UdpSocket, resolver: Ipv4Addr, log: Logger) {
    let mut query = [0u8; DNS_BUFFER_SIZE];
    let mut answer = [0u8; DNS_BUFFER_SIZE];

    loop {
        let (length, client) = match local.recv_from(&mut query) {
            Ok(received) => received,
            Err(_) => return,
        };

        if let Err(err) = host.send(&query[..length]) {
            crate::debug!(
                log,
                "dns: cannot forward query",
                resolver = resolver,
                err = err
            );
            continue;
        }
        match host.recv(&mut answer) {
            Ok(length) => {
                let _ = local.send_to(&answer[..length], client);
            }
            Err(err) => {
                crate::debug!(log, "dns: no answer", resolver = resolver, err = err);
            }
        }
    }
}

/// Reports a relay startup failure without killing the app: DNS is best effort.
pub fn report_relay_error(resolver: Ipv4Addr, err: &str) {
    eprintln!("__dial: dns relay {resolver}: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_resolv_conf() {
        let dir = std::env::temp_dir().join(format!("adhoc-dns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resolv.conf");
        let mut file = std::fs::File::create(&path).unwrap();
        write!(
            file,
            "\n# comment\n; also a comment\nsearch example.com\n\
             nameserver 127.0.0.53\n\
             nameserver 192.168.1.14   # lan dns\n\
             nameserver 127.0.0.53\n\
             nameserver fe80::1\n\
             nameserver bogus\n\
             domain foo\n"
        )
        .unwrap();
        drop(file);

        let got = resolver_addrs(&path);
        assert_eq!(
            got,
            vec![Ipv4Addr::new(127, 0, 0, 53), Ipv4Addr::new(192, 168, 1, 14)]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_resolv_conf_is_not_an_error() {
        assert!(resolver_addrs(Path::new("/nonexistent/resolv.conf")).is_empty());
    }

    #[test]
    fn opens_one_socket_per_resolver() {
        let relays =
            prepare_dns_relays(&[Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]).unwrap();
        assert_eq!(relays.len(), 2);
        assert_eq!(relays[0].resolver, Ipv4Addr::new(1, 1, 1, 1));
    }

    #[test]
    fn configured_servers_win_over_resolv_conf() {
        let cfg = ServiceConfig {
            dns_servers: vec!["9.9.9.9".to_string(), "fe80::1".to_string()],
            ..Default::default()
        };
        let log = Logger::new(false);
        assert_eq!(
            resolve_dns_targets(&cfg, &log),
            vec![Ipv4Addr::new(9, 9, 9, 9)]
        );

        let cfg = ServiceConfig {
            dns: "off".to_string(),
            ..Default::default()
        };
        assert!(resolve_dns_targets(&cfg, &log).is_empty());
    }

    #[test]
    fn formats_ip_lists() {
        assert_eq!(
            format_ips(&[Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]),
            "1.1.1.1,8.8.8.8"
        );
    }

    #[test]
    fn resolves_a_query_through_the_relay() {
        // A fake upstream resolver that answers with the query bytes reversed.
        let upstream = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut buffer = [0u8; 512];
            let (length, peer) = upstream.recv_from(&mut buffer).unwrap();
            let mut reply = buffer[..length].to_vec();
            reply.reverse();
            upstream.send_to(&reply, peer).unwrap();
        });

        let host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        host.connect(upstream_addr).unwrap();
        host.set_read_timeout(Some(DNS_QUERY_TIMEOUT)).unwrap();

        let local = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let local_addr = local.local_addr().unwrap();
        let log = Logger::new(false);
        std::thread::spawn(move || relay_queries(local, host, Ipv4Addr::LOCALHOST, log));

        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.send_to(b"abcdef", local_addr).unwrap();

        let mut answer = [0u8; 64];
        let (length, _) = client.recv_from(&mut answer).unwrap();
        assert_eq!(&answer[..length], b"fedcba");
        server.join().unwrap();
    }
}
