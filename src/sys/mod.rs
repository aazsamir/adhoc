//! The raw kernel interfaces — the only module allowed to contain `unsafe`.
//!
//! The rest of the program is ordinary safe Rust. Everything that needs a raw
//! descriptor, a raw syscall or an `extern "C"` handler lives here, and the crate
//! root enforces that with `#![deny(unsafe_code)]` plus `#[allow(unsafe_code)]`
//! on this module tree.
//!
//! Read it in this order:
//!
//! * [`socket`] — the `SO_REUSEADDR` bind the launcher needs, socketpairs,
//!   pipes, and `poll`.
//! * [`fd`] — adopting raw descriptors, and passing them to another process with
//!   `SCM_RIGHTS` or an inherited fd number.
//! * [`proc`] — spawning/`exec`, signals, and waiting for a child.
//! * [`ns`] — `unshare`, `setns`, bringing `lo` up, the route probe.
//! * [`netlink`] — just enough rtnetlink to add an address to an interface.
//! * [`clock`] — the wall clock the log lines need.
//!
//! Each file's `unsafe` blocks state the contract they rely on. If you are
//! reading the code to understand *how adhoc works*, you can skip this module
//! tree entirely and start at [`crate::app`].

#![allow(unsafe_code)]

pub mod clock;
pub mod fd;
pub mod netlink;
pub mod ns;
pub mod proc;
pub mod socket;

use std::io;

/// Turns a libc return value into an `io::Result`, retrying nothing.
///
/// This is the single place that knows libc reports failure as `-1` plus
/// `errno`, so the rest of the module can stay readable.
pub fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// The same for the `ssize_t`-returning calls (`read`, `sendto`, ...).
pub fn cvt_syscall(result: libc::ssize_t) -> io::Result<libc::ssize_t> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
