# adhoc

Lazy service launcher + TCP proxy. Declare services in YAML; the first connection to a service's port starts the app, traffic is proxied to it, and after `idle_timeout` without traffic the app is stopped. Apps keep binding their own port — they run in a private network namespace, no root required.

Code internals: [ARCHITECTURE.md](ARCHITECTURE.md).

## Build

```bash
cargo build --release
./target/release/adhoc verify --config adhoc.yaml   # check config, namespaces, ports, binaries
```

Requires: Linux with unprivileged user namespaces, util-linux `unshare` (when not root), `passt` only for `net: pasta`.

## Run

```bash
adhoc run    --config adhoc.yaml   # -c short; default path adhoc.yaml; --debug for details
adhoc verify --config adhoc.yaml
adhoc example                      # print a starter config
```

- All listen sockets are bound at startup — port conflicts fail at boot, not on first request.
- A burst of cold connections starts the app once; the rest wait.
- A crashed app is reaped and restarted on the next request.
- `SIGINT`/`SIGTERM`/`SIGHUP` stop everything cleanly.

systemd:

```ini
[Service]
ExecStart=/usr/local/bin/adhoc run --config /etc/adhoc.yaml
Restart=on-failure
KillMode=control-group
```

## Config

```yaml
log_dir: logs

services:
  - name: web
    listen: 127.0.0.1:8080        # string or list, see below
    internal_port: 8080           # port the app binds (default: listen port)
    command: ["python3", "-m", "http.server", "{port}", "--bind", "127.0.0.1"]
    workdir: /srv/web
    env:
      NODE_ENV: production
    isolation: netns              # netns (default) | port
    net: none                     # none | pasta | slirp4netns | custom
    dns: auto                     # auto | off
    idle_timeout: 10m             # 0 = never stop
    startup_timeout: 30s
    stop_timeout: 10s             # SIGTERM grace before SIGKILL
    connect_timeout: 10s          # dial timeout for isolation: port
    net_timeout: 5s
    log: file                     # file (log_dir/<name>.log) | inherit | discard
```

- `{port}` is replaced with the app port in `command` and `env` values.
- Durations: `30s`, `1h30m`, `250ms`, or bare seconds. Unknown keys are rejected.

### Multiple ports

`listen` takes a list. Each entry proxies to the app port with the same number, or `host:port=appport` for an explicit mapping. One app instance serves all ports; the app stops only when *all* ports are quiet for `idle_timeout`.

```yaml
listen:
  - 0.0.0.0:8889        # -> app 8889
  - 127.0.0.1:9000=8080 # host 9000 -> app 8080
```

### isolation

- `netns` (default): app runs in a private netns and keeps its hardcoded port/bind.
- `port`: no namespaces; app is told to bind a different port via `{port}`. Use for apps that need real network access and accept a configurable port.

### net (outbound network for netns apps)

| `net:` | what runs | notes |
|---|---|---|
| `none` (default) | nothing | loopback only |
| `pasta` | `pasta` | recommended; copies host addresses/routes; needs `passt` + `/dev/net/tun` |
| `slirp4netns` | `slirp4netns` | own `10.0.2.0/24` subnet; IPv6 off unless `net_args: ["--enable-ipv6"]` |
| `custom` | `net_command` verbatim | `{netns}`, `{pid}` expanded |

### dns

`dns: auto` (default) relays the host's resolver into the namespace (loopback and LAN resolvers both work). `dns: off` disables; `dns_servers: [..]` overrides what is relayed.

## Limitations

- TCP only — UDP ports cannot be proxied.
- Services cannot reach each other's loopback; chain through the proxy's listen address.
- `net: pasta` does not work when adhoc itself runs as root (use `slirp4netns` or `custom`).


## AI Disclaimer

This project is fully vibe-coded, use at your own risk.

## License

MIT