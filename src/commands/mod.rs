//! The user-facing commands.
//!
//! * [`run`] — `adhoc run` (and the internal `__serve`): bind, maybe re-exec
//!   into a user namespace, then hand everything to [`crate::app::App`].
//! * [`reexec`] — the user-namespace re-exec and the descriptor hand-over that
//!   goes with it. This is the hard part of `run`; `run` itself is just CLI.
//! * [`verify`] — `adhoc verify`: check the config and this machine.
//! * [`example`] — `adhoc example`: print a starter config.

pub mod example;
pub mod reexec;
pub mod run;
pub mod verify;
