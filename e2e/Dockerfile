# Prepared environment for the adhoc end-to-end suite.
#
# Everything the suite needs to exercise the real binary against real
# namespaces:
#   - Rust toolchain (build only)
#   - util-linux `unshare` (rootless user-namespace re-exec)
#   - iproute2 + procps (diagnostics, /proc access)
#   - python3 (the app under test)
#   - passt/pasta (outbound network tests; they skip if /dev/net/tun is missing)
#
# Both the launcher and the test binary are prebuilt so the image can be run as
# an unprivileged user (--user 1000:1000) without a writable target directory.
#
# Build with --build-arg INSTRUMENT=1 for the coverage flavour: both binaries are
# compiled with `-C instrument-coverage` and llvm-tools is installed, so the
# profiles can be merged afterwards (see e2e/coverage.sh).

ARG RUST_VERSION=1.95
FROM rust:${RUST_VERSION}

ARG INSTRUMENT=0

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      iproute2 \
      passt \
      procps \
      python3 \
      util-linux \
 && rm -rf /var/lib/apt/lists/*

RUN if [ "$INSTRUMENT" = "1" ]; then rustup component add llvm-tools-preview; fi

WORKDIR /src

# The e2e scripts are copied early: editing them must not invalidate the
# expensive compile layer below.
COPY e2e/docker-entrypoint.sh e2e/coverage.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh /usr/local/bin/coverage.sh

# Warm the dependency cache in its own layer: source changes then only rebuild
# this crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo fetch --locked \
 && rm -rf src

COPY . .

# One build produces the release launcher, the dev launcher that `cargo test`
# bakes into the test binary as CARGO_BIN_EXE_adhoc, and the test binary.
RUN set -eux; \
    cargo test --no-run --test lifecycle --locked --message-format=json > /tmp/artifacts.json; \
    cargo build --locked; \
    if [ "$INSTRUMENT" = "1" ]; then export RUSTFLAGS="-C instrument-coverage"; fi; \
    cargo build --release --locked; \
    test_bin="$(python3 -c "import json; print([a['executable'] for a in map(json.loads, open('/tmp/artifacts.json')) if a.get('executable') and a['profile'].get('test')][0])")"; \
    cp "$test_bin" /usr/local/bin/adhoc-e2e; \
    cp target/release/adhoc /usr/local/bin/adhoc; \
    chmod +x /usr/local/bin/adhoc-e2e /usr/local/bin/adhoc; \
    rm -f /tmp/artifacts.json

# The suite reads ADHOC_BIN, so it always exercises the release build.
ENV ADHOC_BIN=/usr/local/bin/adhoc

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["--test-threads=4"]

# Run the image with --privileged (root pass) or --user 1000:1000 (rootless
# pass); see e2e/run.sh.
