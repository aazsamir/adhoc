# End-to-end suite

These tests run the **real launcher binary** against real namespaces, in a
container prepared with everything the suite needs. Nothing is mocked: the
config is parsed, ports are bound, apps are spawned in private network
namespaces, file descriptors are passed over `SCM_RIGHTS` and bytes are proxied
end to end.

They exist so that a change anywhere (lifecycle, fd passing, namespace setup,
config parsing, CLI) has to survive the whole pipeline before it ships.

## What is covered

| test | checks |
|---|---|
| `lazy_start_proxy_idle_stop_and_restart` | nothing runs before the first connection, the first request starts the app, idle stop closes the app port while the proxy keeps listening, the next request restarts it, SIGTERM shuts down cleanly |
| `a_burst_of_cold_connections_starts_the_app_once` | 8 concurrent cold connections produce exactly one app start |
| `large_payloads_survive_the_proxy` | 2 MiB round trip through the proxy, byte for byte |
| `netns_services_can_share_an_internal_port` | two services bind the *same* internal port in their own namespaces, each reachable through its own host port, and that internal port is not bound on the host |
| `netns_service_proxies_every_listen_port` | one app, several host ports, single instance |
| `netns_app_without_outbound_network_has_only_loopback` | `net: none` really isolates: connecting from inside the namespace to the proxy's host port fails |
| `netns_app_can_reach_the_outside_with_pasta` | `net: pasta` gives the namespace a route (skips without `pasta` or `/dev/net/tun`) |
| `sigterm_stops_a_netns_service_and_its_namespace` | SIGTERM reaches the launcher through the user-namespace supervisor and the app dies with it |
| `verify_reports_problems_and_accepts_good_configs` | `verify` output, duplicate names, missing config, occupied ports |
| `the_example_config_is_valid` | `example` prints something the parser and `verify` accept |
| `run_fails_fast_on_a_port_conflict` | a taken port fails at boot, not on the first request |
| `unknown_commands_and_bad_map_entries_are_rejected` | CLI error paths |

## Running it

```bash
./e2e/run.sh                 # root pass + rootless pass (recommended)
./e2e/run.sh --root          # root only
./e2e/run.sh --nonroot       # uid 1000: exercises `unshare --user`
./e2e/run.sh -- --test-threads=1  # forward extra flags to the test binary
make e2e                     # same as ./e2e/run.sh
make e2e-coverage            # instrumented run + llvm-cov report
```

Or with compose:

```bash
docker compose -f e2e/docker-compose.yml run --rm e2e
docker compose -f e2e/docker-compose.yml --profile nonroot run --rm e2e-nonroot
```

The container runs `--privileged` because the suite unshares user and network
namespaces and, for the pasta test, opens `/dev/net/tun`. The rootless pass runs
as uid 1000 with no capabilities at all, which is the path a normal user takes.

## The image

`e2e/Dockerfile` builds on the official Rust image and adds `unshare`,
`iproute2`, `python3` and `passt`, then prebuilds three artifacts inside the
image:

* `/usr/local/bin/adhoc` — the release binary, which is what the tests execute
  (via `ADHOC_BIN`), so the e2e suite tests the artifact you would install;
* `target/debug/adhoc` — needed because `cargo test` bakes that path into the
  test binary;
* `/usr/local/bin/adhoc-e2e` — the test binary itself.

Everything is baked in, so the image can be run as an unprivileged user without
a writable target directory, and no compilation happens at test time.

Build with `--build-arg INSTRUMENT=1` to get the coverage flavour: both binaries
are compiled with `-C instrument-coverage`, the test run writes `.profraw` files
(inherited by every process adhoc spawns), and `e2e/coverage.sh` merges them and
reports line coverage for both binaries.

## Notes

* `ADHOC_BIN=/path/to/adhoc cargo test --test lifecycle` runs the same suite
  locally against any build.
* Tests that need a feature the environment lacks (`pasta`, `/dev/net/tun`, or
  running as root where passt cannot attach) print a `skipping:` line and pass,
  so the suite is never broken by a missing kernel feature. `run.sh` always
  passes `--nocapture` so those lines are visible: a skip must never look like a
  silent pass.
