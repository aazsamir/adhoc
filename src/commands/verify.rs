//! `adhoc verify`: check the config and this machine without starting anything.
//!
//! Every check prints one line, `[ok  ]` or `[FAIL]`, and the command exits
//! non-zero if any of them failed. It answers the questions that would otherwise
//! only show up as a failed first request:
//!
//! * does the config parse and is it self-consistent?
//! * can this machine do unprivileged user namespaces at all?
//! * are the commands and net helpers installed?
//! * is every listen port free?

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cli::Args;
use crate::commands::run::{DEFAULT_CONFIG, uses_namespaces};
use crate::config::{Config, DnsMode, Isolation, ListenSpec, NetMode, ServiceConfig};
use crate::log::Logger;
use crate::netns::dns::{format_ips, resolve_dns_targets};
use crate::netns::nethelper::net_argv;
use crate::program;
use crate::sys::proc::{euid, wait_timeout};
use crate::sys::socket::bind_tcp;

/// How long `unshare -r -n true` gets before we call it broken.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn cmd_verify(args: &[String]) -> i32 {
    let parsed = Args::parse(args, &["config"]);
    let config_path = parsed.value_or("config", DEFAULT_CONFIG);

    let cfg = match Config::load(Path::new(config_path)) {
        Ok(cfg) => cfg,
        Err(err) => {
            println!("[FAIL] config: {err:#}");
            return 1;
        }
    };

    let mut report = Report::default();
    report.ok(format!(
        "config {config_path} parses ({} services)",
        cfg.services.len()
    ));

    verify_namespace_support(&mut report, &cfg);
    for service in &cfg.services {
        verify_service(&mut report, service);
    }

    if report.failures > 0 {
        println!("\n{} problem(s) found", report.failures);
        return 1;
    }
    println!("\nall good");
    0
}

/// Collects the lines and counts the failures.
#[derive(Default)]
struct Report {
    failures: usize,
}

impl Report {
    fn ok(&mut self, message: impl AsRef<str>) {
        println!("[ok  ] {}", message.as_ref());
    }

    fn fail(&mut self, message: impl AsRef<str>) {
        println!("[FAIL] {}", message.as_ref());
        self.failures += 1;
    }

    /// Prints an extra, indented line of context under a failure.
    fn detail(&self, message: impl AsRef<str>) {
        println!("       {}", message.as_ref());
    }
}

/// Checks the one thing that can make every netns service fail at once: no
/// working unprivileged user namespaces.
fn verify_namespace_support(report: &mut Report, cfg: &Config) {
    if !uses_namespaces(cfg) {
        report.ok("no service uses isolation=netns, namespaces not needed");
        return;
    }
    if euid() == 0 {
        report.ok("running as root - namespaces available directly");
        return;
    }

    if program::exists("unshare") {
        report.ok("util-linux `unshare` available");
    } else {
        report.fail("util-linux `unshare` available");
    }

    match run_with_timeout("unshare", &["-r", "-n", "true"]) {
        Ok(()) => report.ok("unprivileged user+net namespaces work"),
        Err(err) => {
            report.fail("unprivileged user+net namespaces work");
            report.detail(format!("{err:#}"));
            report.detail("check: cat /proc/sys/kernel/unprivileged_userns_clone");
        }
    }
}

fn verify_service(report: &mut Report, service: &ServiceConfig) {
    let specs = service.listen_specs().unwrap_or_default();
    report.ok(format!(
        "service {:<12} {} isolation={}",
        service.name,
        describe_bindings(&specs),
        service.isolation_mode()
    ));

    match service.command.first() {
        None => report.fail(format!("  {}: command is required", service.name)),
        Some(app) if !program::exists(app) => report.fail(format!(
            "  {}: command {app:?} not found in PATH",
            service.name
        )),
        Some(_) => {}
    }

    if !service.workdir.is_empty() && !Path::new(&service.workdir).is_dir() {
        report.fail(format!(
            "  {}: workdir {:?} missing",
            service.name, service.workdir
        ));
    }

    verify_service_network(report, service);

    for spec in &specs {
        verify_port_is_free(report, &service.name, &spec.addr);
    }
}

fn verify_service_network(report: &mut Report, service: &ServiceConfig) {
    if service.net_mode() == NetMode::None || service.isolation_mode() != Isolation::Netns {
        return;
    }

    match net_argv(service, "/proc/self/ns/net") {
        Err(err) => report.fail(format!("  {}: {err:#}", service.name)),
        Ok(argv) if !program::exists(&argv[0]) => report.fail(format!(
            "  {}: net: {} needs {:?} (pacman -S passt)",
            service.name, service.net, argv[0]
        )),
        Ok(argv) => report.ok(format!(
            "  {}: net: {} via {}",
            service.name, service.net, argv[0]
        )),
    }

    if service.dns_mode() == DnsMode::Auto {
        let servers = resolve_dns_targets(service, &Logger::new(false));
        let line = format!("  {}: dns relay for {}", service.name, format_ips(&servers));
        if servers.is_empty() {
            report.fail(line);
        } else {
            report.ok(line);
        }
    }
}

fn verify_port_is_free(report: &mut Report, service: &str, addr: &str) {
    match bind_tcp(addr) {
        Ok(listener) => {
            drop(listener);
            report.ok(format!("  {service}: {addr} is free"));
        }
        Err(err) => report.fail(format!("  {service}: cannot bind {addr}: {err:#}")),
    }
}

fn describe_bindings(specs: &[ListenSpec]) -> String {
    specs
        .iter()
        .map(|spec| format!("{}->{}", spec.addr, spec.app_port))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Runs a command with a time limit; used for the namespace probe.
fn run_with_timeout(program: &str, args: &[&str]) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| program.to_string())?;

    match wait_timeout(&mut child, PROBE_TIMEOUT).with_context(|| program.to_string())? {
        Some(status) if status.success() => Ok(()),
        Some(status) => bail!("{program} exited with {status}"),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out")
        }
    }
}
