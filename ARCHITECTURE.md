# Architecture / source map

This is the "where do I start?" guide. User-facing behaviour is in
[README.md](README.md); this file is about the code.

adhoc is one Rust crate: a small `lib.rs`, a `main.rs` that only dispatches, and
modules grouped by *how hard they are to understand*. If you are reading the code
for the first time, stop after the first five files below — that is the whole
program. The rest is plumbing that only matters when it breaks.

```
src/
├── lib.rs            module map + reading order (you are reading its words here)
├── main.rs           argv -> command
│
├── config/           ── the easy half: what the program means ──
│   ├── mod.rs          Config, ServiceConfig, validation
│   ├── duration.rs     30s / 1h30m / 30 as a YAML value
│   └── listen.rs       host:port=appport, {port}, app ports
├── app.rs            owns the listeners, accepts connections, idle ticker
├── service/          one lazily started app
│   ├── mod.rs          the lifecycle state machine (start here)
│   ├── process.rs      spawn / reap / SIGTERM->SIGKILL
│   └── readiness.rs    "is it accepting yet?"
├── sandbox/          how the app is kept apart from the proxy
│   ├── mod.rs          the Sandbox trait + factory
│   ├── port.rs         isolation: port   (none)
│   └── netns.rs        isolation: netns  (private loopback)
├── proxy.rs          io::copy in both directions
│
├── netns/            ── the hard half: one network namespace per service ──
│   ├── mod.rs          what the pieces are, in reading order
│   ├── protocol.rs     3-byte requests between manager and helper
│   ├── dialer.rs       manager side of the fd-passing helper
│   ├── dialer_child.rs the helper itself (`adhoc __dial`)
│   ├── spawn.rs        `adhoc __spawn`: setns + exec in place
│   ├── nethelper.rs    pasta / slirp4netns / custom outbound network
│   └── dns.rs          resolv.conf relay into the namespace
├── sys/              ── the raw kernel interfaces, the only `unsafe` ──
│   ├── mod.rs          `cvt`, and the rule: unsafe lives here only
│   ├── socket.rs       SO_REUSEADDR bind, socketpair, pipe, poll
│   ├── fd.rs           adopt fds, SCM_RIGHTS, dup2-before-exec
│   ├── proc.rs         spawn/exec, signals, waiting
│   ├── ns.rs           unshare, setns, lo up, route probe
│   ├── netlink.rs      RTM_NEWADDR (add an address to lo)
│   └── clock.rs        the wall clock for log lines
│
├── commands/         the CLI entry points
│   ├── run.rs          `run` / `__serve`
│   ├── reexec.rs       the user-namespace re-exec + descriptor hand-over
│   ├── verify.rs       `verify`
│   └── example.rs      `example`
├── cli.rs            `--config x`, `--config=x`, `-c x`
├── log.rs            one-line logger + info!/warn!/debug!/log_error!
├── sync.rs           Latch (one-shot flag) + `lock` that ignores poisoning
├── program.rs        self_path(), in_path()
└── tailwriter.rs     remembers the last lines a helper printed
```

## Reading order for newcomers

1. `config/` — what a config means, what is valid. No concurrency, no Linux.
2. `app.rs` — the long-lived part: listeners, accept loops, idle ticker.
3. `service/mod.rs` — the interesting part. Read the state machine at the top,
   then follow `ensure_running` down.
4. `sandbox/mod.rs` — the two strategies in one table. Then `port.rs` (trivial)
   and *optionally* `netns.rs` (glue).
5. `proxy.rs` — it really is just `io::copy` in both directions.

Everything else can wait:

* `netns/` — only needed when you want to know how a connection reaches an app in
  another network namespace. Its `mod.rs` explains the whole dance in six lines.
* `sys/` — only needed when you wonder "how do we call that syscall". Note that
  **all** `unsafe` is here, which `src/lib.rs` enforces with
  `#![deny(unsafe_code)]` plus `#[allow(unsafe_code)]` on this module tree.
* `commands/reexec.rs` — the one genuinely surprising control-flow in the program
  (we re-execute ourselves through `unshare`).

## Data flow

### Startup

```
commands::run::cmd_run
 ├─ config::Config::load            parse + validate
 ├─ app::bind                       reserve every host port now
 └─ rootless and netns needed?
      ├─ no:  app::App::run
      └─ yes: commands::reexec::restart
               └─ unshare --user --map-root-user -- adhoc __serve --map 3:0:8080
                    └─ commands::run::cmd_serve   (fd 3 is the listener)
```

### One connection

```
accept_loop (app.rs)
 └─ Service::handle(client, app_port)
      ├─ begin_request                    count it (blocks the idle stop)
      ├─ ensure_running                   start the app once, wait for ready
      │    ├─ sandbox.start               netns: dialer + net helper + DNS
      │    ├─ AppProcess::spawn           {port} expanded, own process group
      │    ├─ readiness::wait_until_ready every app port must accept
      │    └─ state = Running
      ├─ sandbox.dial(app_port)
      │    └─ netns: ask the helper, receive the fd with SCM_RIGHTS
      └─ proxy::splice(client, upstream)
```

`RequestGuard` decrements `inflight` when the connection is done, which is also
what resets the idle timer.

### Idle stop

`app::idle_loop` ticks every 250 ms and calls `Service::stop_if_idle`:

```
running? inflight == 0? idle >= idle_timeout?
  yes -> AppProcess::stop: SIGTERM, wait stop_timeout, SIGKILL, wait
         then sandbox.stop(), state = Stopped
```

### Crash

The reaper thread inside `AppProcess` raises its `Latch` first (a stop may be
waiting on it), then calls back into `Service::after_exit`, which takes the
lifecycle lock, marks the service stopped and tears the sandbox down. The next
connection starts it again.

## Concurrency rules

- `Service::lifecycle` is locked for the whole of a start or a stop. That is what
  makes a burst of cold connections start the app **exactly once**.
- It is **never** held while bytes are flowing through a connection.
- `state`, `inflight` and `last_used` are atomics, so `running()`, `inflight()`
  and the idle check never block behind a slow start.
- The child process lives in the reaper thread (`wait` needs ownership); the
  service keeps only its pid and a `Latch`. The latch is raised *before* the
  reaper takes the lifecycle lock, otherwise a stop holding the lock while
  waiting for that latch would deadlock.
- `Sandbox::dial` clones the dialer out of the sandbox state before dialing, so a
  concurrent stop can tear the namespace down without yanking the socket from
  under an in-flight connection.
- Accept loops never block in `accept()`: they `poll` with a 200 ms timeout and
  check the shutdown flag, which is how shutdown interrupts them without closing
  a socket from another thread.
- Nothing that runs after `fork` allocates: `sys::fd::inherit_as` only calls
  `dup2`/`fcntl`.

## The internal commands

The binary re-executes itself because namespace syscalls are per-thread and must
run before any thread is spawned:

| command | why |
|---|---|
| `__serve` | the real launcher, inside the user namespace made by `unshare` |
| `__dial` | owns a service's netns, hands connected sockets to the manager |
| `__spawn` | joins that netns and `exec`s the app in place (same pid) |

`netns/protocol.rs` documents the tiny manager <-> `__dial` wire format. File
descriptors are plain integers: fd 3 is the control socketpair, fd 4+ are
host-side DNS sockets.

## Dependencies

| crate | why |
|---|---|
| `libc` | the syscalls std does not wrap (confined to `sys/`) |
| `serde` | derive the config types |
| `serde-saphyr` | YAML parsing (pure Rust, panic-free, errors with line/column) |
| `anyhow` | error handling: `Result<T>` everywhere, `.context()` for what failed |

No async runtime, no CLI parser, no logging framework. Toolchain: edition 2024,
`rust-version = 1.95` (current stable), enforced by cargo.

Error messages are meant to be read, so anything user-facing prints `{err:#}` —
the alternate form shows the whole context chain (`parse adhoc.yaml: unknown field
'lisen'`), not just the outermost layer.

## Testing map

| tests | what they cover |
|---|---|
| `config::tests`, `config::{duration,listen}::tests` | YAML decoding, durations, listen parsing, defaults, validation |
| `cli::tests` | both dash styles, repeated flags, `--` separator |
| `service::tests` | lifecycle via a fake sandbox: burst, idle, crash, escalation, env/workdir |
| `service::process::tests` | spawn, reap, SIGTERM->SIGKILL escalation |
| `service::readiness::tests` | ports, timeouts, process exit |
| `app::tests` | binding, log files, port conflicts |
| `sandbox::tests` | factory, isolation description, dialing |
| `netns::dialer::tests` | fd-passing protocol, framing, error paths |
| `netns::dns::tests` | resolv.conf parsing, relay round trip |
| `netns::nethelper::tests` | helper argv for pasta/slirp/custom |
| `commands::reexec::tests` | `--map` encoding/decoding round trip |
| `sys::*::tests` | bind, `SCM_RIGHTS` round trip, exec inheritance, netlink encoding |
| `proxy::tests` | both directions, half-close |
| `sync::tests` | the latch, poisoning |
| `tailwriter::tests` | line buffering, CRLF, concurrent writers |
| `tests/lifecycle.rs` | the real binary: lazy start, idle stop, netns isolation, SIGTERM, CLI |
| `e2e/` | the same suite in a prepared Docker image, as root and as uid 1000, plus llvm-cov coverage |
