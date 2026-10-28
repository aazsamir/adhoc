//! `adhoc __spawn --netns-pid PID -- CMD...` joins the network namespace owned
//! by the dialer helper and then exec()s the app *in place*. Because of the
//! in-place exec, the pid the manager tracks is the app itself, not a wrapper.

use crate::cli::Args;
use crate::sys::{ns, proc};

/// Joins another process' network namespace and replaces this process with the
/// app. The manager tracks our pid, which after the exec *is* the app's pid — no
/// wrapper in between to signal, wait for, or leak.
pub fn cmd_spawn(args: &[String]) -> i32 {
    let parsed = Args::parse(args, &["netns-pid"]);
    let netns_pid: i32 = parsed
        .value("netns-pid")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let argv = parsed.rest;
    if netns_pid == 0 || argv.is_empty() {
        eprintln!("__spawn: --netns-pid PID -- CMD... required");
        return 2;
    }

    if let Err(err) = ns::enter_netns(netns_pid) {
        eprintln!("__spawn: enter the namespace of pid {netns_pid}: {err}");
        return 1;
    }

    // `exec` only returns on failure.
    let err = proc::exec(&argv);
    eprintln!("__spawn: exec {}: {err}", argv[0]);
    if err.raw_os_error() == Some(libc::ENOENT) {
        127 // the convention for "command not found"
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn missing_arguments_are_a_usage_error() {
        assert_eq!(cmd_spawn(&argv(&[])), 2);
        assert_eq!(cmd_spawn(&argv(&["--netns-pid", "0", "--", "true"])), 2);
        assert_eq!(cmd_spawn(&argv(&["--netns-pid", "1"])), 2);
    }

    #[test]
    fn unknown_namespaces_fail_cleanly() {
        // pid 1 exists but its namespace cannot be joined here, and a bogus pid
        // definitely fails to open.
        let code = cmd_spawn(&argv(&["--netns-pid", "4194303", "--", "/bin/true"]));
        assert_eq!(code, 1);
    }
}
