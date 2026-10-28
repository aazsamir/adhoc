//! The per-service network-namespace machinery.
//!
//! Everything in here exists to answer one question: *how does a process in the
//! host namespace talk to an app that lives in its own network namespace?*
//!
//! The answer is a small dance, and this is the only place that performs it:
//!
//! 1. [`dialer`] starts a helper process (`adhoc __dial`) that unshares a fresh
//!    network namespace and keeps it alive. The manager never enters it.
//! 2. [`spawn`] (`adhoc __spawn`) joins that namespace and `exec`s the app, so
//!    the pid the manager tracks *is* the app.
//! 3. [`protocol`] is the three-byte request the manager sends to the helper
//!    when it needs a connection: the helper dials inside the namespace and
//!    hands the connected socket back with `SCM_RIGHTS`.
//! 4. [`nethelper`] optionally attaches a userspace network stack (pasta or
//!    slirp4netns) so the app can reach the outside world.
//! 5. [`dns`] relays the host's resolvers into the namespace for the same
//!    reason.
//!
//! The pieces are documented in the order you should read them: protocol,
//! dialer, dialer_child, spawn, then nethelper and dns.

pub mod dialer;
pub mod dialer_child;
pub mod dns;
pub mod nethelper;
pub mod protocol;
pub mod spawn;
