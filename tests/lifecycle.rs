//! End-to-end tests against the real binary.
//!
//! They exercise the whole stack: config parsing, binding, accept loops, the
//! lazy start, the fd-passing dialer, `__spawn`, idle stop and shutdown. The app
//! under test is python3's http.server (and a small inline echo server), so no
//! fixtures beyond a Python interpreter are needed.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);

/// The binary under test. `ADHOC_BIN` lets the Docker suite point at an
/// installed (or coverage-instrumented) build instead of cargo's dev binary.
fn bin() -> String {
    std::env::var("ADHOC_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_adhoc").to_string())
}

// --- helpers ----------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn scratch_dir(name: &str) -> PathBuf {
    let unique = format!(
        "adhoc-test-{}-{}-{}-{name}",
        std::process::id(),
        Instant::now().elapsed().as_nanos(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

fn port_is_open(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

fn namespaces_available() -> bool {
    Command::new("unshare")
        .args(["-r", "-n", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A launcher process plus the config and logs it was started with.
struct Launcher {
    child: Child,
    dir: PathBuf,
    config: PathBuf,
    /// Everything the launcher wrote to stderr, collected as it arrives.
    stderr: Arc<Mutex<String>>,
}

impl Launcher {
    fn start(config_body: &str, dir: &Path) -> Launcher {
        let config = dir.join("adhoc.yaml");
        std::fs::write(&config, config_body).unwrap();

        let mut command = Command::new(bin());
        command
            .arg("run")
            .arg("--config")
            .arg(&config)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group: the launcher re-executes itself through
        // `unshare`, so cleanup has to signal the whole group.
        command.process_group(0);
        let mut child = command.spawn().expect("spawn adhoc");

        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let buffer = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(length) => buffer
                            .lock()
                            .unwrap()
                            .push_str(&String::from_utf8_lossy(&chunk[..length])),
                    }
                }
            });
        }

        Launcher {
            child,
            dir: dir.to_path_buf(),
            config,
            stderr,
        }
    }

    fn wait_for_listen(&self, port: u16) {
        wait_until("the launcher to listen", TIMEOUT, || port_is_open(port));
    }

    /// Waits for a line on the launcher's own log (stderr).
    fn wait_for_log(&self, needle: &str) {
        wait_until("a log line", TIMEOUT, || self.stderr().contains(needle));
    }

    fn signal(&self, signal: libc::c_int) {
        unsafe { libc::kill(self.child.id() as i32, signal) };
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.child.try_wait().unwrap() {
                Some(status) => return Some(status.code().unwrap_or(-1)),
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        }
        None
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn logs(&self) -> String {
        let logs = self.dir.join("logs");
        let mut combined = String::new();
        if let Ok(entries) = std::fs::read_dir(&logs) {
            for entry in entries.flatten() {
                combined.push_str(&format!(
                    "--- {} ---\n{}\n",
                    entry.path().display(),
                    std::fs::read_to_string(entry.path()).unwrap_or_default()
                ));
            }
        }
        combined
    }

    fn check_alive(&mut self) {
        if let Some(status) = self.child.try_wait().unwrap() {
            panic!(
                "launcher exited early with {status}\n\nconfig:\n{}\n\nstderr:\n{}\n\nlogs:\n{}",
                std::fs::read_to_string(&self.config).unwrap_or_default(),
                self.stderr(),
                self.logs()
            );
        }
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        let group = -(self.child.id() as i32);
        // Ask nicely first: that is what tears the apps down.
        unsafe { libc::kill(group, libc::SIGTERM) };
        if self.wait_exit(Duration::from_secs(10)).is_none() {
            unsafe { libc::kill(group, libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

/// Sends one HTTP/1.0 request and returns (status line, body).
fn http_get(port: u16, path: &str) -> std::io::Result<(String, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.write_all(format!("GET {path} HTTP/1.0\r\nHost: x\r\n\r\n").as_bytes())?;
    stream.flush()?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let text = String::from_utf8_lossy(&response).to_string();
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((head, body)) => (head.to_string(), body.to_string()),
        None => (text, String::new()),
    };
    let status = head.lines().next().unwrap_or_default().to_string();
    Ok((status, body))
}

/// An app that records every start and then serves HTTP from `dir`.
fn http_app_command(port: u16, site: &Path, record: &Path) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "echo start >> {}; exec python3 -m http.server {port} --bind 127.0.0.1 --directory {}",
            record.display(),
            site.display()
        ),
    ]
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|body| body.lines().count())
        .unwrap_or(0)
}

fn yaml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("{:?}", item)).collect();
    format!("[{}]", quoted.join(", "))
}

// --- port isolation ---------------------------------------------------------

#[test]
fn lazy_start_proxy_idle_stop_and_restart() {
    if !python_available() {
        eprintln!("skipping: python3 is not available");
        return;
    }

    let dir = scratch_dir("port");
    let site = dir.join("site");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(site.join("index.html"), "hello from the app").unwrap();
    let record = dir.join("starts.log");

    let listen = free_port();
    let app_port = free_port();
    let command = http_app_command(app_port, &site, &record);

    let config = format!(
        r#"log_dir: {logs}
services:
  - name: web
    listen: 127.0.0.1:{listen}
    internal_port: {app_port}
    command: {command}
    isolation: port
    idle_timeout: 500ms
    startup_timeout: 20s
    stop_timeout: 2s
"#,
        logs = dir.join("logs").display(),
        listen = listen,
        app_port = app_port,
        command = yaml_list(&command),
    );

    let mut launcher = Launcher::start(&config, &dir);
    launcher.wait_for_listen(listen);
    launcher.check_alive();

    // Nothing runs before the first connection.
    assert_eq!(count_lines(&record), 0, "the app started too early");
    assert!(!port_is_open(app_port), "the app port must be closed");

    let (status, body) = http_get(listen, "/").expect("first request");
    assert!(status.contains("200"), "status was {status:?}");
    assert!(body.contains("hello from the app"), "body was {body:?}");
    assert_eq!(count_lines(&record), 1, "exactly one cold start");
    assert!(port_is_open(app_port));

    // The proxy keeps listening while the app is stopped for being idle.
    wait_until("the app to be stopped while idle", TIMEOUT, || {
        !port_is_open(app_port)
    });
    launcher.check_alive();
    assert!(port_is_open(listen), "the proxy must keep listening");

    // The next request starts a fresh instance.
    let (status, _) = http_get(listen, "/").expect("second request");
    assert!(status.contains("200"), "status was {status:?}");
    assert_eq!(count_lines(&record), 2, "the app was restarted once");

    // SIGTERM stops everything cleanly.
    let pid = launcher.child.id() as i32;
    launcher.signal(libc::SIGTERM);
    let code = launcher
        .wait_exit(TIMEOUT)
        .unwrap_or_else(|| panic!("launcher did not exit\n{}", launcher.logs()));
    assert_eq!(code, 0, "clean exit\n{}", launcher.logs());

    // The app dies with the launcher.
    wait_until("the app to be torn down", TIMEOUT, || {
        (unsafe { libc::kill(pid, 0) } != 0) && !port_is_open(app_port)
    });
}

#[test]
fn a_burst_of_cold_connections_starts_the_app_once() {
    if !python_available() {
        eprintln!("skipping: python3 is not available");
        return;
    }

    let dir = scratch_dir("burst");
    let site = dir.join("site");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(site.join("index.html"), "burst").unwrap();
    let record = dir.join("starts.log");

    let listen = free_port();
    let app_port = free_port();
    let command = http_app_command(app_port, &site, &record);

    let config = format!(
        r#"log_dir: {logs}
services:
  - name: web
    listen: 127.0.0.1:{listen}
    internal_port: {app_port}
    command: {command}
    isolation: port
    idle_timeout: 0
    startup_timeout: 20s
    stop_timeout: 2s
"#,
        logs = dir.join("logs").display(),
        listen = listen,
        app_port = app_port,
        command = yaml_list(&command),
    );

    let mut launcher = Launcher::start(&config, &dir);
    launcher.wait_for_listen(listen);

    let mut clients = Vec::new();
    for _ in 0..8 {
        clients.push(std::thread::spawn(move || http_get(listen, "/")));
    }
    for client in clients {
        let (status, body) = client.join().unwrap().expect("request");
        assert!(status.contains("200"), "status was {status:?}");
        assert_eq!(body.trim(), "burst");
    }

    assert_eq!(count_lines(&record), 1, "a burst must start the app once");
    launcher.check_alive();
}

#[test]
fn large_payloads_survive_the_proxy() {
    if !python_available() {
        eprintln!("skipping: python3 is not available");
        return;
    }

    let dir = scratch_dir("payload");
    let listen = free_port();
    let app_port = free_port();

    let echo = format!(
        "import socket\n\
         s = socket.socket()\n\
         s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         s.bind(('127.0.0.1', {app_port}))\n\
         s.listen(8)\n\
         while True:\n\
         \x20   c, _ = s.accept()\n\
         \x20   while True:\n\
         \x20       d = c.recv(65536)\n\
         \x20       if not d: break\n\
         \x20       c.sendall(d)\n\
         \x20   c.close()\n"
    );
    let command = vec!["python3".to_string(), "-c".to_string(), echo.clone()];

    let config = format!(
        r#"log_dir: {logs}
services:
  - name: echo
    listen: 127.0.0.1:{listen}
    internal_port: {app_port}
    command: {command}
    isolation: port
    idle_timeout: 0
    startup_timeout: 20s
"#,
        logs = dir.join("logs").display(),
        listen = listen,
        app_port = app_port,
        command = yaml_list(&command),
    );

    let mut launcher = Launcher::start(&config, &dir);
    launcher.wait_for_listen(listen);

    let payload: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();

    let mut stream = TcpStream::connect(("127.0.0.1", listen)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let write_payload = payload.clone();
    let writer = std::thread::spawn(move || {
        stream.write_all(&write_payload).unwrap();
        stream.flush().unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        stream
    });

    let mut stream = writer.join().unwrap();
    let mut echoed = Vec::with_capacity(payload.len());
    let mut buffer = [0u8; 65536];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(length) => echoed.extend_from_slice(&buffer[..length]),
            Err(err) => panic!("read: {err}"),
        }
    }
    assert_eq!(echoed.len(), payload.len(), "payload length changed");
    assert!(echoed == payload, "payload was corrupted");
    launcher.check_alive();
}

// --- network namespaces -----------------------------------------------------

#[test]
fn netns_services_can_share_an_internal_port() {
    if !namespaces_available() || !python_available() {
        eprintln!("skipping: unprivileged namespaces or python3 unavailable");
        return;
    }

    let dir = scratch_dir("netns-shared");
    let listen_a = free_port();
    let listen_b = free_port();
    let internal = free_port();

    let config = format!(
        r#"log_dir: {logs}
services:
  - name: a
    listen: 127.0.0.1:{listen_a}
    internal_port: {internal}
    command: ["python3", "-m", "http.server", "{{port}}", "--bind", "127.0.0.1"]
    workdir: {site_a}
    isolation: netns
    idle_timeout: 0
    startup_timeout: 20s
  - name: b
    listen: 127.0.0.1:{listen_b}
    internal_port: {internal}
    command: ["python3", "-m", "http.server", "{{port}}", "--bind", "127.0.0.1"]
    workdir: {site_b}
    isolation: netns
    idle_timeout: 0
    startup_timeout: 20s
"#,
        logs = dir.join("logs").display(),
        site_a = make_site(&dir, "a", "service-a").display(),
        site_b = make_site(&dir, "b", "service-b").display(),
        internal = internal,
    );

    let mut launcher = Launcher::start(&config, &dir);
    launcher.wait_for_listen(listen_a);
    launcher.wait_for_listen(listen_b);

    let (status_a, body_a) = http_get(listen_a, "/").expect("service a");
    assert!(status_a.contains("200"), "a: {status_a}");
    assert!(body_a.contains("service-a"), "a body: {body_a}");

    let (status_b, body_b) = http_get(listen_b, "/").expect("service b");
    assert!(status_b.contains("200"), "b: {status_b}");
    assert!(body_b.contains("service-b"), "b body: {body_b}");

    // Both apps keep the same internal port, in their own namespaces.
    assert!(
        !port_is_open(internal),
        "the internal port must stay private"
    );
    launcher.check_alive();
}

#[test]
fn netns_service_proxies_every_listen_port() {
    if !namespaces_available() || !python_available() {
        eprintln!("skipping: unprivileged namespaces or python3 unavailable");
        return;
    }

    let dir = scratch_dir("netns-multi");
    let one = free_port();
    let two = free_port();
    let internal = free_port();

    let config = format!(
        r#"log_dir: {logs}
services:
  - name: multi
    listen: ["127.0.0.1:{one}", "127.0.0.1:{two}"]
    internal_port: {internal}
    command: ["python3", "-m", "http.server", "{{port}}", "--bind", "127.0.0.1"]
    workdir: {site}
    isolation: netns
    idle_timeout: 0
    startup_timeout: 20s
"#,
        logs = dir.join("logs").display(),
        site = make_site(&dir, "multi", "multi-port").display(),
        internal = internal,
    );

    let mut launcher = Launcher::start(&config, &dir);
    launcher.wait_for_listen(one);
    launcher.wait_for_listen(two);

    for port in [one, two] {
        let (status, body) = http_get(port, "/").expect("request");
        assert!(status.contains("200"), "port {port}: {status}");
        assert!(body.contains("multi-port"), "port {port} body: {body}");
    }
    launcher.check_alive();
}

#[test]
fn netns_app_without_outbound_network_has_only_loopback() {
    if !namespaces_available() || !python_available() {
        eprintln!("skipping: unprivileged namespaces or python3 unavailable");
        return;
    }

    let dir = scratch_dir("netns-isolated");
    let listen = free_port();
    let app_port = free_port();

    // The app tries to connect to the proxy's own host port: inside the private
    // namespace nothing listens on that address, which proves the isolation.
    let probe = format!(
        "import socket\n\
         s = socket.socket()\n\
         s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         s.bind(('127.0.0.1', {app_port}))\n\
         s.listen(8)\n\
         while True:\n\
         \x20   c, _ = s.accept()\n\
         \x20   out = b'isolated'\n\
         \x20   try:\n\
         \x20       p = socket.create_connection(('127.0.0.1', {listen}), timeout=1)\n\
         \x20       out = b'leaked'\n\
         \x20       p.close()\n\
         \x20   except OSError:\n\
         \x20       pass\n\
         \x20   c.sendall(out)\n\
         \x20   c.close()\n"
    );
    let command = vec!["python3".to_string(), "-c".to_string(), probe];

    let config = format!(
        r#"log_dir: {logs}
services:
  - name: probe
    listen: 127.0.0.1:{listen}
    internal_port: {app_port}
    command: {command}
    isolation: netns
    net: none
    idle_timeout: 0
    startup_timeout: 20s
"#,
        logs = dir.join("logs").display(),
        listen = listen,
        app_port = app_port,
        command = yaml_list(&command),
    );

    let mut launcher = Launcher::start(&config, &dir);
    launcher.wait_for_listen(listen);

    let mut stream = TcpStream::connect(("127.0.0.1", listen)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut answer = [0u8; 32];
    let mut received = 0;
    while received < 8 {
        match stream.read(&mut answer[received..]) {
            Ok(0) => break,
            Ok(length) => received += length,
            Err(err) => panic!("read: {err}"),
        }
    }
    assert_eq!(&answer[..received], b"isolated");
    launcher.check_alive();
}

#[test]
fn netns_app_can_reach_the_outside_with_pasta() {
    let pasta_ready = Command::new("pasta")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !namespaces_available() || !pasta_ready {
        eprintln!("skipping: unprivileged namespaces or pasta unavailable");
        return;
    }
    if !std::path::Path::new("/dev/net/tun").exists() {
        eprintln!("skipping: pasta needs /dev/net/tun");
        return;
    }
    if unsafe { libc::geteuid() } == 0 {
        // passt drops privileges (including CAP_SYS_PTRACE) when it starts as
        // root, and then cannot open /proc/<helper>/ns/net any more. Running the
        // launcher as a normal user is the supported mode; the container suite
        // covers this in its rootless pass.
        eprintln!("skipping: passt cannot attach to a namespace when started as root");
        return;
    }

    let dir = scratch_dir("pasta");
    let listen = free_port();
    let app_port = free_port();

    // Reports whether the namespace has a route to the outside at all: a UDP
    // connect needs a routing table entry but sends no packets, exactly like the
    // launcher's own readiness probe.
    let probe = format!(
        "import socket\n\
         s = socket.socket()\n\
         s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         s.bind(('127.0.0.1', {app_port}))\n\
         s.listen(8)\n\
         while True:\n\
         \x20   c, _ = s.accept()\n\
         \x20   u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)\n\
         \x20   try:\n\
         \x20       u.connect(('1.1.1.1', 53))\n\
         \x20       out = b'routable'\n\
         \x20   except OSError:\n\
         \x20       out = b'no-route'\n\
         \x20   u.close()\n\
         \x20   c.sendall(out)\n\
         \x20   c.close()\n"
    );
    let command = vec!["python3".to_string(), "-c".to_string(), probe];

    let config = format!(
        r#"log_dir: {logs}
services:
  - name: online
    listen: 127.0.0.1:{listen}
    internal_port: {app_port}
    command: {command}
    isolation: netns
    net: pasta
    dns: auto
    net_timeout: 5s
    idle_timeout: 0
    startup_timeout: 20s
"#,
        logs = dir.join("logs").display(),
        listen = listen,
        app_port = app_port,
        command = yaml_list(&command),
    );

    let mut launcher = Launcher::start(&config, &dir);
    launcher.wait_for_listen(listen);

    // The first connection starts the app, and startup waits for the helper.
    let mut stream = TcpStream::connect(("127.0.0.1", listen)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut answer = [0u8; 16];
    let mut received = 0;
    while received < 8 {
        match stream.read(&mut answer[received..]) {
            Ok(0) => break,
            Ok(length) => received += length,
            Err(err) => panic!("read: {err}\n{}", launcher.stderr()),
        }
    }
    assert_eq!(&answer[..received], b"routable", "{}", launcher.stderr());
    launcher.wait_for_log("outbound network ready");
    launcher.check_alive();
}

#[test]
fn sigterm_stops_a_netns_service_and_its_namespace() {
    if !namespaces_available() || !python_available() {
        eprintln!("skipping: unprivileged namespaces or python3 unavailable");
        return;
    }

    let dir = scratch_dir("sigterm");
    let listen = free_port();
    let app_port = free_port();
    let pid_file = dir.join("app.pid");

    let script = format!(
        "echo $$ > {pid}; exec python3 -m http.server {app_port} --bind 127.0.0.1",
        pid = pid_file.display()
    );
    let command = vec!["sh".to_string(), "-c".to_string(), script];

    let config = format!(
        r#"log_dir: {logs}
services:
  - name: web
    listen: 127.0.0.1:{listen}
    internal_port: {app_port}
    command: {command}
    isolation: netns
    idle_timeout: 0
    startup_timeout: 20s
    stop_timeout: 3s
"#,
        logs = dir.join("logs").display(),
        command = yaml_list(&command),
    );

    let mut launcher = Launcher::start(&config, &dir);
    launcher.wait_for_listen(listen);

    let (status, _) = http_get(listen, "/").expect("request");
    assert!(status.contains("200"), "{status}");

    let app_pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        (unsafe { libc::kill(app_pid, 0) }) == 0,
        "the app is running"
    );

    launcher.signal(libc::SIGTERM);
    let code = launcher
        .wait_exit(TIMEOUT)
        .unwrap_or_else(|| panic!("launcher did not exit\n{}", launcher.stderr()));
    assert_eq!(code, 0, "clean exit, stderr:\n{}", launcher.stderr());

    wait_until("the app to die with the launcher", TIMEOUT, || {
        (unsafe { libc::kill(app_pid, 0) }) != 0
    });
}

// --- CLI --------------------------------------------------------------------

/// Runs the CLI and returns `(exit code, stdout)`.
fn run_cli_stdout(args: &[&str]) -> (i32, String) {
    let output = Command::new(bin()).args(args).output().expect("run adhoc");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
    )
}

fn run_cli(args: &[&str]) -> (i32, String) {
    let output = Command::new(bin()).args(args).output().expect("run adhoc");
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), text)
}

#[test]
fn verify_reports_problems_and_accepts_good_configs() {
    let dir = scratch_dir("cli");
    let listen = free_port();
    let app_port = free_port();

    let good = dir.join("good.yaml");
    std::fs::write(
        &good,
        format!(
            "log_dir: {logs}\nservices:\n  - name: ok\n    listen: 127.0.0.1:{listen}\n    \
             internal_port: {app_port}\n    command: [\"sleep\", \"30\"]\n    isolation: port\n",
            logs = dir.join("logs").display(),
        ),
    )
    .unwrap();

    let (code, output) = run_cli(&["verify", "--config", good.to_str().unwrap()]);
    assert_eq!(code, 0, "verify failed:\n{output}");
    assert!(output.contains("all good"), "{output}");
    assert!(output.contains("is free"), "{output}");

    let bad = dir.join("bad.yaml");
    std::fs::write(
        &bad,
        "services:\n  - name: a\n    listen: 127.0.0.1:1234\n    command: [\"sleep\", \"1\"]\n  \
         - name: a\n    listen: 127.0.0.1:1235\n    command: [\"sleep\", \"1\"]\n",
    )
    .unwrap();
    let (code, output) = run_cli(&["verify", "--config", bad.to_str().unwrap()]);
    assert_ne!(code, 0, "duplicate names must fail:\n{output}");
    assert!(output.contains("FAIL"), "{output}");

    let (code, output) = run_cli(&["verify", "--config", "/nonexistent/nope.yaml"]);
    assert_ne!(code, 0);
    assert!(output.contains("config"), "{output}");
}

#[test]
fn the_example_config_is_valid() {
    let dir = scratch_dir("example");
    let path = dir.join("example.yaml");

    // stdout only: `example` prints the config there and nothing else.
    let (code, config) = run_cli_stdout(&["example"]);
    assert_eq!(code, 0);
    assert!(config.contains("services:"), "{config}");
    std::fs::write(&path, &config).unwrap();

    let (code, output) = run_cli(&["verify", "--config", path.to_str().unwrap()]);
    assert_eq!(code, 0, "the example config must verify:\n{output}");
    assert!(output.contains("all good"), "{output}");

    let cfg = adhoc::config::Config::load(&path).expect("example config parses");
    assert_eq!(cfg.services.len(), 1);
    assert_eq!(cfg.services[0].name, "web");
}

#[test]
fn run_fails_fast_on_a_port_conflict() {
    let dir = scratch_dir("conflict");
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();

    let config = dir.join("conflict.yaml");
    std::fs::write(
        &config,
        format!(
            "log_dir: {logs}\nservices:\n  - name: clash\n    listen: 127.0.0.1:{port}\n    \
             internal_port: 1\n    command: [\"sleep\", \"5\"]\n    isolation: port\n",
            logs = dir.join("logs").display()
        ),
    )
    .unwrap();

    let output = Command::new(bin())
        .args(["run", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("bind"), "{text}");
}

#[test]
fn unknown_commands_and_bad_map_entries_are_rejected() {
    let (code, output) = run_cli(&["nonsense"]);
    assert_eq!(code, 2);
    assert!(output.contains("unknown command"), "{output}");

    let (code, _) = run_cli(&["__spawn"]);
    assert_eq!(code, 2);

    let (code, _) = run_cli(&["__serve", "--config", "/nonexistent/x.yaml"]);
    assert_eq!(code, 1);
}

// --- small helpers used above -----------------------------------------------

fn make_site(dir: &Path, name: &str, body: &str) -> PathBuf {
    let site = dir.join(name);
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(site.join("index.html"), body).unwrap();
    site
}
