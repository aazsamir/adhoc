//! Spawning, watching and stopping one app process.
//!
//! Three things make this file worth isolating:
//!
//! * the app runs in **its own process group**, so one signal reaches the app and
//!   anything it started;
//! * something has to **reap** the child (`wait`) without anybody blocking on it,
//!   so a reaper thread is started per process;
//! * stopping is a **negotiation**: SIGTERM, wait, SIGKILL, wait.
//!
//! The latch is raised by the reaper *before* it reports the exit, because a
//! [`crate::service::Service::stop`] may be holding the service lock while
//! waiting for exactly that latch.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::log::Logger;
use crate::sync::Latch;
use crate::sys::proc::{kill_group, terminate_group};

/// How long we wait for a `SIGKILL`ed process to actually disappear.
const KILL_WAIT: Duration = Duration::from_secs(5);

/// The app process a service currently owns.
pub struct AppProcess {
    pid: i32,
    exit: Arc<Latch>,
}

impl AppProcess {
    /// Spawns `command` and starts watching it.
    ///
    /// `on_exit(pid, status)` runs on the reaper thread once the child is gone
    /// and the latch is already set.
    pub fn spawn(
        mut command: Command,
        on_exit: impl FnOnce(i32, ExitStatus) + Send + 'static,
    ) -> io::Result<AppProcess> {
        command.stdin(Stdio::null());
        // Its own process group: `kill(-pid, ...)` then reaches the whole tree.
        command.process_group(0);

        let mut child = command.spawn()?;
        let pid = child.id() as i32;
        let exit = Arc::new(Latch::new());

        let latch = Arc::clone(&exit);
        thread::spawn(move || {
            let status = reap(&mut child);
            latch.signal();
            on_exit(pid, status);
        });

        Ok(AppProcess { pid, exit })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The latch that is set as soon as the process is gone.
    pub fn exit(&self) -> Arc<Latch> {
        Arc::clone(&self.exit)
    }

    /// Stops the process politely, then not so politely.
    pub fn stop(&self, grace: Duration, log: &Logger) {
        terminate_group(self.pid);
        if self.exit.wait_timeout(grace) {
            return;
        }

        crate::warn!(log, "app ignored SIGTERM, killing", pid = self.pid);
        kill_group(self.pid);
        if !self.exit.wait_timeout(KILL_WAIT) {
            crate::log_error!(log, "app did not die", pid = self.pid);
        }
    }
}

/// `Child::wait`, without letting an error lose the fact that we are done.
///
/// `wait` only fails if the child was already reaped, which cannot happen here;
/// treat it as "exited with status 1" rather than propagating a spurious error
/// into the reaper thread, where there is nobody to report it to.
fn reap(child: &mut Child) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    child
        .wait()
        .unwrap_or_else(|_| ExitStatus::from_raw(1 << 8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::lock;
    use std::sync::Mutex;

    fn logger() -> Logger {
        Logger::new(false)
    }

    #[test]
    fn a_spawned_process_is_reaped_and_reported() {
        let exits: Arc<Mutex<Vec<i32>>> = Arc::default();
        let seen = Arc::clone(&exits);

        let mut command = Command::new("sh");
        command.args(["-c", "exit 3"]);
        let process = AppProcess::spawn(command, move |pid, status| {
            assert_eq!(status.code(), Some(3));
            lock(&seen).push(pid);
        })
        .unwrap();

        assert!(process.pid() > 0);
        assert!(process.exit().wait_timeout(Duration::from_secs(10)));
        assert_eq!(lock(&exits).as_slice(), &[process.pid()]);
    }

    #[test]
    fn stop_escalates_when_sigterm_is_ignored() {
        let mut command = Command::new("sh");
        command.args(["-c", "trap '' TERM; sleep 30"]);
        let process = AppProcess::spawn(command, |_, _| {}).unwrap();

        let started = std::time::Instant::now();
        process.stop(Duration::from_millis(100), &logger());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(process.exit().is_set());
    }

    #[test]
    fn stop_returns_immediately_for_a_cooperative_app() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let process = AppProcess::spawn(command, |_, _| {}).unwrap();

        let started = std::time::Instant::now();
        process.stop(Duration::from_secs(5), &logger());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_missing_binary_is_a_clean_error() {
        let err = AppProcess::spawn(Command::new("definitely-not-a-real-binary"), |_, _| {});
        assert!(err.is_err());
    }
}
