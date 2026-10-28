//! Command dispatch.
//!
//! The public commands are `run`, `verify` and `example`. The `__serve`,
//! `__dial` and `__spawn` commands are internal: adhoc re-executes itself with
//! them to enter namespaces (see README.md).

use std::process::ExitCode;

use adhoc::commands::{example, run, verify};
use adhoc::netns::{dialer_child, spawn};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Installed for every command: `run` forwards signals to its user-namespace
    // child, `__serve` turns them into a clean shutdown.
    adhoc::sys::proc::install_signal_handlers();

    let code = match args.first().map(String::as_str) {
        None => {
            usage();
            2
        }
        Some("run") => run::cmd_run(&args[1..]),
        Some("verify") => verify::cmd_verify(&args[1..]),
        Some("example") => {
            example::cmd_example();
            0
        }
        Some("__serve") => run::cmd_serve(&args[1..]),
        Some("__dial") => dialer_child::cmd_dial(&args[1..]),
        Some("__spawn") => spawn::cmd_spawn(&args[1..]),
        Some("-h") | Some("--help") | Some("help") => {
            usage();
            0
        }
        Some(other) => {
            eprintln!("unknown command {other:?}");
            usage();
            2
        }
    };

    ExitCode::from(code.clamp(0, 255) as u8)
}

fn usage() {
    eprintln!(
        r#"adhoc - lazy launcher + smart TCP proxy

usage:
  adhoc run    --config <file.yaml>    start the launcher
  adhoc verify --config <file.yaml>    check the config and this machine
  adhoc example                        print an example config

Every service gets a host port. The first connection to that port spawns the app
in its own network namespace, proxies the connection to it, and after
idle_timeout without traffic the app is stopped again. No root needed.
"#
    );
}
