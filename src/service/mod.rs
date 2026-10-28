//! One lazily started app.
//!
//! This is the heart of adhoc: a small state machine per service.
//!
//! ```text
//!                ensure_running()
//!   Stopped ──────────────────────► Starting ──ready──► Running
//!      ▲                              │                   │
//!      └───────── failure ────────────┘                   │
//!      └────────────── stop / crash ──────────────────────┘
//! ```
//!
//! The rules that keep it correct, and that are worth knowing before changing
//! anything here:
//!
//! * The `lifecycle` mutex is held for the whole of a start or a stop. That is
//!   what makes a burst of cold connections start the app **exactly once**.
//! * The lock is **never** held while bytes are flowing through a connection.
//! * The observable state (`state`, `inflight`, `last_used`) is atomic, so
//!   `running()` and the idle check never block behind a slow start.
//!
//! The two parts that are easy to get wrong live next door: `process` spawns and
//! stops the child (signal escalation), and `readiness` polls the app's ports
//! until they answer.

mod process;
mod readiness;

use std::fs::{File, OpenOptions};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::config::{LogMode, ServiceConfig, expand};
use crate::log::Logger;
use crate::proxy::splice;
use crate::sandbox::{Sandbox, new_sandbox};
use crate::sync::lock;

use self::process::AppProcess;

/// How long after becoming ready an app is allowed to die before we assume
/// something else is holding its port.
const SUSPICIOUS_UPTIME: Duration = Duration::from_secs(2);

/// What the outside world can observe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Stopped,
    Starting,
    Running,
    Stopping,
}

/// An [`AtomicU8`] that only ever holds a [`State`], so call sites read as
/// `self.state.get() == State::Running` instead of comparing magic numbers.
struct AtomicState(AtomicU8);

impl AtomicState {
    fn new(state: State) -> AtomicState {
        AtomicState(AtomicU8::new(state as u8))
    }

    fn get(&self) -> State {
        match self.0.load(Ordering::SeqCst) {
            1 => State::Starting,
            2 => State::Running,
            3 => State::Stopping,
            _ => State::Stopped,
        }
    }

    fn set(&self, state: State) {
        self.0.store(state as u8, Ordering::SeqCst);
    }
}

/// Everything that belongs to "the app process we currently own, if any".
/// Guarded by [`Service::lifecycle`].
#[derive(Default)]
struct Lifecycle {
    process: Option<AppProcess>,
    /// When the app last became ready; used for the "died right away" warning.
    ready_at: Option<Instant>,
}

pub struct Service {
    cfg: ServiceConfig,
    log: Logger,
    sandbox: Box<dyn Sandbox>,

    /// Where the app's stdout/stderr go when `log: file`.
    app_log: Mutex<Option<File>>,

    /// Serialises start and stop. Never held while connection bytes flow.
    lifecycle: Mutex<Lifecycle>,

    state: AtomicState,
    /// Connections currently being proxied.
    inflight: AtomicI64,
    /// Unix milliseconds when the last connection finished.
    last_used: AtomicI64,
}

impl Service {
    pub fn new(cfg: ServiceConfig, log_dir: &str, log: &Logger) -> Result<Service> {
        let sandbox = new_sandbox(&cfg, log)?;
        Service::with_sandbox(cfg, sandbox, log_dir, log)
    }

    /// Shared by [`Service::new`] and the tests, which inject a fake sandbox.
    fn with_sandbox(
        cfg: ServiceConfig,
        sandbox: Box<dyn Sandbox>,
        log_dir: &str,
        log: &Logger,
    ) -> Result<Service> {
        let app_log = match cfg.log_mode() {
            LogMode::File => Some(open_app_log(&cfg.name, log_dir)?),
            _ => None,
        };

        Ok(Service {
            log: log.with_service(&cfg.name),
            cfg,
            sandbox,
            app_log: Mutex::new(app_log),
            lifecycle: Mutex::default(),
            state: AtomicState::new(State::Stopped),
            inflight: AtomicI64::new(0),
            last_used: AtomicI64::new(now_millis()),
        })
    }

    pub fn cfg(&self) -> &ServiceConfig {
        &self.cfg
    }

    pub fn running(&self) -> bool {
        self.state.get() == State::Running
    }

    /// The app's pid, or 0 when it is not running.
    pub fn pid(&self) -> i32 {
        lock(&self.lifecycle)
            .process
            .as_ref()
            .map_or(0, AppProcess::pid)
    }

    pub fn inflight(&self) -> i64 {
        self.inflight.load(Ordering::SeqCst)
    }

    // --- connection accounting ----------------------------------------------

    fn begin_request(&self) {
        self.inflight.fetch_add(1, Ordering::SeqCst);
    }

    fn end_request(&self) {
        if self.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.last_used.store(now_millis(), Ordering::SeqCst);
        }
    }

    // --- start --------------------------------------------------------------

    /// Starts the app if it is not running and returns once it accepts
    /// connections. Concurrent callers wait for the first start instead of
    /// spawning the app several times.
    pub fn ensure_running(self: &Arc<Self>) -> Result<()> {
        let mut lifecycle = lock(&self.lifecycle);
        if self.state.get() == State::Running {
            return Ok(());
        }
        self.start(&mut lifecycle)
    }

    fn start(self: &Arc<Self>, lifecycle: &mut Lifecycle) -> Result<()> {
        self.state.set(State::Starting);
        let started_at = Instant::now();
        crate::info!(self.log, "starting", isolation = self.sandbox.describe());

        let app_log = lock(&self.app_log)
            .as_ref()
            .map(File::try_clone)
            .transpose()
            .context("clone the app log file")?;

        if let Err(err) = self.sandbox.start(app_log.as_ref()) {
            crate::log_error!(self.log, "sandbox setup failed", err = format!("{err:#}"));
            return Err(self.fail_start(err));
        }

        let command = match self.command(app_log.as_ref()) {
            Ok(command) => command,
            Err(err) => return Err(self.fail_start(err)),
        };

        // The reaper reports back to this service; that is why the callback
        // needs an `Arc<Service>` rather than a plain `&self`.
        let this = Arc::clone(self);
        let process =
            match AppProcess::spawn(command, move |pid, status| this.after_exit(pid, status)) {
                Ok(process) => process,
                Err(err) => {
                    crate::log_error!(self.log, "spawn failed", err = err);
                    return Err(self.fail_start(err.into()));
                }
            };

        let exited = process.exit();
        lifecycle.process = Some(process);

        if let Err(err) = readiness::wait_until_ready(
            |port| self.sandbox.dial(port),
            &self.cfg.app_ports(),
            self.cfg.startup().as_std(),
            &exited,
        ) {
            crate::log_error!(
                self.log,
                "app not ready",
                err = format!("{err:#}"),
                after = crate::config::Duration::from(started_at.elapsed())
            );
            self.stop_process(lifecycle);
            return Err(err);
        }

        lifecycle.ready_at = Some(Instant::now());
        self.last_used.store(now_millis(), Ordering::SeqCst);
        self.state.set(State::Running);
        crate::info!(
            self.log,
            "ready",
            took = crate::config::Duration::from(started_at.elapsed())
        );
        Ok(())
    }

    /// Undoes a half-finished start and hands the error back, so callers can
    /// write `return Err(self.fail_start(err))`.
    fn fail_start(&self, err: anyhow::Error) -> anyhow::Error {
        self.sandbox.stop();
        self.state.set(State::Stopped);
        err
    }

    /// Builds the app's command line: `{port}` expanded, the sandbox wrapper
    /// applied, workdir/env/stdio set up.
    fn command(&self, app_log: Option<&File>) -> Result<Command> {
        let port = self.cfg.primary_app_port();
        let argv: Vec<String> = self
            .cfg
            .command
            .iter()
            .map(|arg| expand(arg, port))
            .collect();
        let argv = self.sandbox.app_command(&argv);

        let (program, args) = argv.split_first().context("command is empty")?;
        let mut command = Command::new(program);
        command.args(args);

        if !self.cfg.workdir.is_empty() {
            // Checked here as well as in `verify`, because "workdir typo" would
            // otherwise surface as a bare ENOENT from spawn.
            if !Path::new(&self.cfg.workdir).is_dir() {
                anyhow::bail!("workdir {} does not exist", self.cfg.workdir);
            }
            command.current_dir(&self.cfg.workdir);
        }
        for (key, value) in &self.cfg.env {
            command.env(key, expand(value, port));
        }

        match self.cfg.log_mode() {
            LogMode::File => {
                let file = app_log.context("log: file needs an open log file")?;
                command
                    .stdout(file.try_clone().context("app log")?)
                    .stderr(file.try_clone().context("app log")?);
            }
            LogMode::Inherit => {
                command.stdout(std::process::Stdio::inherit());
                command.stderr(std::process::Stdio::inherit());
            }
            LogMode::Discard => {
                command.stdout(std::process::Stdio::null());
                command.stderr(std::process::Stdio::null());
            }
        }
        Ok(command)
    }

    /// Runs on the reaper thread after the app is gone.
    ///
    /// The latch has already been raised at this point, so a `stop` that was
    /// waiting for it can be holding the lifecycle lock; we simply queue up
    /// behind it and then discover the process is no longer ours.
    fn after_exit(self: &Arc<Self>, pid: i32, status: std::process::ExitStatus) {
        let mut lifecycle = lock(&self.lifecycle);
        if lifecycle.process.as_ref().map(AppProcess::pid) != Some(pid) {
            return; // stop() already cleaned up
        }

        let was_running = self.state.get() == State::Running;
        let ready_at = lifecycle.ready_at.take();
        lifecycle.process = None;
        self.state.set(State::Stopped);

        if !was_running {
            return; // it died while starting; start() reports that
        }

        crate::warn!(self.log, "app exited", err = status);
        if ready_at.is_some_and(|ready| ready.elapsed() < SUSPICIOUS_UPTIME) {
            crate::warn!(
                self.log,
                "it died right after its port started answering - something else may be \
                 holding its port inside the namespace (see its log file)"
            );
        }
        self.sandbox.stop();
    }

    // --- stop ---------------------------------------------------------------

    /// Stops the app when it has had no traffic for `idle_timeout`.
    pub fn stop_if_idle(&self) {
        let mut lifecycle = lock(&self.lifecycle);

        // A crashed app is already being reaped.
        if lifecycle.process.is_none() || self.state.get() != State::Running {
            return;
        }
        if !self.cfg.idle().is_positive() || self.inflight.load(Ordering::SeqCst) > 0 {
            return;
        }

        let idle_for = now_millis() - self.last_used.load(Ordering::SeqCst);
        if idle_for < self.cfg.idle().millis() {
            return;
        }

        crate::info!(
            self.log,
            "idle, stopping",
            idle_for = crate::config::Duration::from_millis(idle_for)
        );
        self.stop_process(&mut lifecycle);
    }

    /// Stops the app for good and closes its log file.
    pub fn shutdown(&self) {
        {
            let mut lifecycle = lock(&self.lifecycle);
            self.stop_process(&mut lifecycle);
        }
        if let Some(mut file) = lock(&self.app_log).take() {
            let _ = std::io::Write::flush(&mut file);
        }
    }

    /// Expects the caller to hold the lifecycle lock.
    fn stop_process(&self, lifecycle: &mut Lifecycle) {
        // Taking the process out first means the reaper's `after_exit` sees that
        // it is no longer ours and stays quiet.
        if let Some(process) = lifecycle.process.take() {
            self.state.set(State::Stopping);
            process.stop(self.cfg.stop().as_std(), &self.log);
        }

        lifecycle.ready_at = None;
        self.state.set(State::Stopped);
        self.sandbox.stop();
        crate::info!(self.log, "stopped");
    }

    // --- serving one connection ---------------------------------------------

    /// Proxies one accepted host connection to the app's port, starting the app
    /// first if necessary.
    pub fn handle(self: &Arc<Self>, client: TcpStream, app_port: u16) {
        self.begin_request();
        let _finished = RequestGuard(self);

        if let Err(err) = self.ensure_running() {
            crate::log_error!(
                self.log,
                "start failed, dropping connection",
                err = format!("{err:#}")
            );
            return;
        }

        match self.sandbox.dial(app_port) {
            Ok(upstream) => splice(client, upstream),
            Err(err) => crate::log_error!(
                self.log,
                "upstream dial failed",
                app_port = app_port,
                err = format!("{err:#}")
            ),
        }
    }
}

/// Ends the request accounting when the connection is done, however it ends.
struct RequestGuard<'a>(&'a Service);

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.0.end_request();
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Opens (or creates) `log_dir/<name>.log`.
fn open_app_log(name: &str, log_dir: &str) -> Result<File> {
    let log_dir = if log_dir.is_empty() { "logs" } else { log_dir };
    std::fs::create_dir_all(log_dir).with_context(|| format!("{name}: log dir {log_dir}"))?;

    let path = Path::new(log_dir).join(format!("{name}.log"));
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("{name}: log file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Duration as Timeout, ListenList};
    use crate::sys::proc::process_alive;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A sandbox that spawns whatever the config says and pretends the app is
    /// always accepting. Real process handling, fake networking: lifecycle tests
    /// stay fast and dependency-free.
    #[derive(Default)]
    struct FakeSandbox {
        started: AtomicUsize,
        stopped: AtomicUsize,
        start_error: Mutex<Option<String>>,
    }

    impl Sandbox for Arc<FakeSandbox> {
        fn start(&self, _app_log: Option<&File>) -> Result<()> {
            self.started.fetch_add(1, Ordering::SeqCst);
            match lock(&self.start_error).clone() {
                Some(message) => anyhow::bail!(message),
                None => Ok(()),
            }
        }

        fn app_command(&self, argv: &[String]) -> Vec<String> {
            argv.to_vec()
        }

        fn dial(&self, _app_port: u16) -> Result<TcpStream> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            Ok(TcpStream::connect(listener.local_addr()?)?)
        }

        fn stop(&self) {
            self.stopped.fetch_add(1, Ordering::SeqCst);
        }

        fn describe(&self) -> String {
            "fake".to_string()
        }
    }

    fn config(name: &str, command: &[&str], idle_ms: i64) -> ServiceConfig {
        ServiceConfig {
            name: name.to_string(),
            listen: ListenList(vec![format!("127.0.0.1:{}", free_port())]),
            internal_port: 8080,
            command: command.iter().map(|part| part.to_string()).collect(),
            isolation: "port".to_string(),
            idle_timeout: Some(Timeout::from_millis(idle_ms)),
            startup_timeout: Some(Timeout::from_secs(5)),
            stop_timeout: Some(Timeout::from_secs(2)),
            log: "discard".to_string(),
            ..Default::default()
        }
    }

    fn service(name: &str, command: &[&str], idle_ms: i64) -> (Arc<Service>, Arc<FakeSandbox>) {
        let sandbox = Arc::new(FakeSandbox::default());
        let mut cfg = config(name, command, idle_ms);
        cfg.log = "discard".to_string();
        let service =
            Service::with_sandbox(cfg, Box::new(Arc::clone(&sandbox)), "", &Logger::new(false))
                .unwrap();
        (Arc::new(service), sandbox)
    }

    #[test]
    fn a_burst_of_callers_starts_the_app_once() {
        let (service, sandbox) = service("burst", &["sleep", "30"], 0);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let service = Arc::clone(&service);
            handles.push(std::thread::spawn(move || service.ensure_running()));
        }
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        assert_eq!(sandbox.started.load(Ordering::SeqCst), 1, "starts");
        assert!(service.running());
        assert!(process_alive(service.pid()));

        service.shutdown();
        assert!(!service.running());
        assert_eq!(service.pid(), 0);
        assert!(sandbox.stopped.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn sandbox_failures_leave_the_service_stopped() {
        let (service, sandbox) = service("broken", &["sleep", "30"], 0);
        *lock(&sandbox.start_error) = Some("pasta exited immediately".to_string());

        let err = service.ensure_running().unwrap_err();
        assert!(format!("{err:#}").contains("pasta"), "{err:#}");
        assert!(!service.running());
        assert_eq!(service.pid(), 0);
    }

    #[test]
    fn a_spawn_failure_tears_the_sandbox_down() {
        let (service, sandbox) = service("nospawn", &["definitely-not-a-real-binary"], 0);

        assert!(service.ensure_running().is_err());
        assert!(!service.running());
        assert!(
            sandbox.stopped.load(Ordering::SeqCst) >= 1,
            "the sandbox must be torn down"
        );
    }

    #[test]
    fn idle_stop_kills_the_app_after_the_timeout() {
        let (service, sandbox) = service("idle", &["sleep", "30"], 100);
        service.ensure_running().unwrap();
        let pid = service.pid();
        assert!(process_alive(pid));

        // Not idle yet.
        service.stop_if_idle();
        assert!(service.running());

        std::thread::sleep(Duration::from_millis(150));
        service.stop_if_idle();

        assert!(!service.running(), "the idle app must be stopped");
        assert!(!process_alive(pid), "the process must be gone");
        assert!(sandbox.stopped.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn live_connections_block_the_idle_stop() {
        let (service, _sandbox) = service("busy", &["sleep", "30"], 100);
        service.ensure_running().unwrap();

        service.begin_request(); // a long-lived connection is in flight
        std::thread::sleep(Duration::from_millis(150));
        service.stop_if_idle();
        assert!(service.running(), "busy services must not be stopped");

        service.end_request();
        std::thread::sleep(Duration::from_millis(150));
        service.stop_if_idle();
        assert!(!service.running(), "quiet services are stopped");
    }

    #[test]
    fn idle_timeout_zero_never_stops() {
        let (service, _sandbox) = service("forever", &["sleep", "30"], 0);
        service.ensure_running().unwrap();

        std::thread::sleep(Duration::from_millis(150));
        service.stop_if_idle();
        assert!(service.running());
        service.shutdown();
    }

    #[test]
    fn a_crash_is_reaped_and_the_next_request_restarts() {
        let (service, sandbox) = service("crash", &["sh", "-c", "sleep 0.2; exit 3"], 0);
        service.ensure_running().unwrap();
        assert!(service.running());

        let give_up_at = Instant::now() + Duration::from_secs(10);
        while service.running() && Instant::now() < give_up_at {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!service.running(), "a crashed app must be reaped");
        assert_eq!(service.pid(), 0);

        service.ensure_running().unwrap();
        assert_eq!(sandbox.started.load(Ordering::SeqCst), 2, "restarts");
        service.shutdown();
    }

    #[test]
    fn env_and_workdir_reach_the_app() {
        let dir = std::env::temp_dir().join(format!("adhoc-env-{}", free_port()));
        std::fs::create_dir_all(&dir).unwrap();

        let (service, _sandbox) = service(
            "env",
            &["sh", "-c", "printf '%s|%s' \"$ADHOC_PORT\" \"$PWD\" > out"],
            0,
        );
        // The service was built already; rebuild with workdir/env to keep the
        // fixture readable.
        drop(service);

        let mut cfg = config(
            "env",
            &["sh", "-c", "printf '%s|%s' \"$ADHOC_PORT\" \"$PWD\" > out"],
            0,
        );
        cfg.workdir = dir.display().to_string();
        cfg.env
            .insert("ADHOC_PORT".to_string(), "{port}".to_string());
        let sandbox = Arc::new(FakeSandbox::default());
        let service = Arc::new(
            Service::with_sandbox(cfg, Box::new(Arc::clone(&sandbox)), "", &Logger::new(false))
                .unwrap(),
        );

        service.ensure_running().unwrap();
        let give_up_at = Instant::now() + Duration::from_secs(5);
        while !dir.join("out").exists() && Instant::now() < give_up_at {
            std::thread::sleep(Duration::from_millis(20));
        }
        let written = std::fs::read_to_string(dir.join("out")).unwrap();
        assert_eq!(written, format!("8080|{}", dir.display()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn log_files_are_created_in_the_configured_directory() {
        let dir = std::env::temp_dir().join(format!("adhoc-logs-{}", free_port()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut cfg = config("logs", &["sleep", "1"], 1000);
        cfg.log = "file".to_string();
        assert!(Service::new(cfg, dir.to_str().unwrap(), &Logger::new(false)).is_ok());
        assert!(dir.join("logs.log").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
