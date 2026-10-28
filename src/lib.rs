//! adhoc is a lazy launcher + TCP proxy.
//!
//! Declare services in YAML; the first connection to a service's host port
//! starts the app, traffic is proxied to it, and after `idle_timeout` without
//! traffic the app is stopped again. Services can run in a private network
//! namespace so they keep their hardcoded ports (and their own loopback).
//!
//! # Reading order
//!
//! The easy, high-level story is five files:
//!
//! 1. [`config`] — what a config means and what counts as valid.
//! 2. [`app`] — binds the host ports, accepts connections, ticks the idle timer.
//! 3. [`service`] — one app: start on demand, wait for readiness, stop when idle.
//! 4. [`sandbox`] — the two strategies: no isolation, or a private netns.
//! 5. [`proxy`] — copies bytes in both directions. That is all it does.
//!
//! Everything that is *hard* is isolated, and named so you can skip it until you
//! need it:
//!
//! * [`netns`] — the namespace machinery: helper processes, fd passing, DNS and
//!   outbound-network helpers.
//! * [`sys`] — the raw kernel interfaces. This is the only module allowed to
//!   contain `unsafe` code (enforced by `#![deny(unsafe_code)]` below).
//! * [`sync`] — the one-shot latch the lifecycle code waits on, so the tricky
//!   synchronisation is a small type instead of a maze of channels.
//!
//! Small helpers: [`cli`], [`log`], [`tailwriter`], and [`commands`] for the
//! `run` / `verify` / `example` entry points.

// All `unsafe` lives in `sys`, which opts in with `#[allow(unsafe_code)]`.
// Keeping that promise machine-checked is worth more than the occasional
// convenience elsewhere; tests may opt in locally.
#![deny(unsafe_code)]

pub mod app;
pub mod cli;
pub mod commands;
pub mod config;
pub mod log;
pub mod netns;
pub mod program;
pub mod proxy;
pub mod sandbox;
pub mod service;
pub mod sync;
pub mod sys;
pub mod tailwriter;
