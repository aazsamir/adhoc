//! Processes and signals.
//!
//! The launcher manages two kinds of child processes — apps and helpers — and
//! both are started in their own process group so that one `kill(-pid, ...)`
//! reaches the app and anything it spawned.
//!
//! Shutdown is driven by a signal handler that stores an atomic and returns
//! immediately: nothing else is legal in a signal handler, and it is all we need
//! because the accept loops poll that flag.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use super::cvt;

/// The signals that ask the launcher to shut down.
const SHUTDOWN_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    LAST_SIGNAL.store(signal, Ordering::SeqCst);
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Asks the process to stop on SIGINT/SIGTERM/SIGHUP.
pub fn install_signal_handlers() {
    // SAFETY: the handler only stores atomics, which is async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        for signal in SHUTDOWN_SIGNALS {
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// Whether a shutdown signal has arrived.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// The signal that caused the shutdown, if any. Clears it.
pub fn take_shutdown_signal() -> Option<libc::c_int> {
    match LAST_SIGNAL.swap(0, Ordering::SeqCst) {
        0 => None,
        signal => Some(signal),
    }
}

/// Passes a signal on to another process (the supervisor forwards to `__serve`).
pub fn forward_signal(pid: u32, signal: libc::c_int) {
    // SAFETY: plain kill on a pid we spawned.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

/// `SIGTERM` to the process group led by `pid`, falling back to the plain pid.
pub fn terminate_group(pid: i32) {
    signal(pid, libc::SIGTERM);
}

/// `SIGKILL` to the process group led by `pid`, falling back to the plain pid.
pub fn kill_group(pid: i32) {
    signal(pid, libc::SIGKILL);
}

fn signal(pid: i32, signal: libc::c_int) {
    if pid <= 0 {
        return;
    }
    // SAFETY: a negative pid means "the process group led by pid"; both calls
    // are harmless if the process is already gone.
    unsafe {
        if libc::kill(-pid, signal) != 0 {
            libc::kill(pid, signal);
        }
    }
}

/// Whether the process still exists.
pub fn process_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// Waits up to `timeout` for `child` to exit.
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A process exit status as a shell-style number (128 + signal when killed).
pub fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or(1, |signal| 128 + signal)
}

/// Replaces the current process image. Only ever called on the main thread.
pub fn exec(argv: &[String]) -> io::Error {
    let program = CString::new(argv[0].as_bytes()).unwrap_or_default();
    let args: Vec<CString> = argv
        .iter()
        .map(|arg| CString::new(arg.as_bytes()).unwrap_or_default())
        .collect();
    let mut pointers: Vec<*const libc::c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
    pointers.push(std::ptr::null());

    // SAFETY: `pointers` is a NULL-terminated argv of valid C strings; execvp
    // does not return on success.
    unsafe { libc::execvp(program.as_ptr(), pointers.as_ptr()) };
    io::Error::last_os_error()
}

/// The effective uid: the difference between "root, namespaces directly" and
/// "rootless, re-exec through `unshare --user`".
pub fn euid() -> u32 {
    // SAFETY: geteuid cannot fail and touches no memory.
    unsafe { libc::geteuid() }
}

/// An open, empty file for `Stdio::null()`-style behaviour that we cannot get
/// from std alone (used where a child must not inherit our stdio).
pub fn dev_null() -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
}

/// `fcntl(F_DUPFD_CLOEXEC)`, exposed for tests that need a spare descriptor.
pub fn duplicate(fd: libc::c_int) -> io::Result<libc::c_int> {
    cvt(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn exit_codes_survive_the_translation() {
        let status = Command::new("sh").args(["-c", "exit 7"]).status().unwrap();
        assert_eq!(exit_code(status), 7);

        let status = Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .status()
            .unwrap();
        assert_eq!(exit_code(status), 137);
    }

    #[test]
    fn wait_timeout_reports_a_slow_child() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        assert!(
            wait_timeout(&mut child, Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
        kill_group(child.id() as i32);
        let _ = child.wait();
    }

    #[test]
    fn process_alive_tracks_death() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(process_alive(pid));
        kill_group(pid);
        let _ = child.wait();
        assert!(!process_alive(pid));
    }

    #[test]
    fn terminating_an_unknown_pid_is_not_an_error() {
        terminate_group(4_194_303);
        kill_group(0);
    }

    #[test]
    fn duplicates_are_close_on_exec() {
        let fd = duplicate(1).unwrap();
        assert!(super::super::fd::is_cloexec(fd));
        super::super::fd::close(fd);
    }
}
