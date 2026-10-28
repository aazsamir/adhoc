#!/usr/bin/env bash
# Runs the end-to-end suite in the prepared Docker image.
#
#   ./e2e/run.sh                 # root pass, then the rootless pass
#   ./e2e/run.sh --root          # root pass only
#   ./e2e/run.sh --nonroot       # uid 1000 only (exercises the unshare --user path)
#   ./e2e/run.sh --coverage      # instrumented run + coverage report
#   ./e2e/run.sh --no-build      # reuse the existing image
#   ./e2e/run.sh -- --nocapture  # extra arguments go to the test binary
#
# The container must be privileged: the suite unshares user and network
# namespaces, mounts /proc data and (for pasta) opens /dev/net/tun.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

IMAGE="${E2E_IMAGE:-adhoc-e2e:local}"
COVERAGE_IMAGE="${E2E_COVERAGE_IMAGE:-adhoc-e2e:coverage}"
MIN_COVERAGE="${MIN_COVERAGE:-70}"
ROOT_PASS=1
NONROOT_PASS=1
BUILD=1
COVERAGE=0
# --nocapture so the "skipping: ..." lines are visible: a silently skipped test
# must not look like a passing one.
TEST_ARGS=("--test-threads=4" "--nocapture")

while [ $# -gt 0 ]; do
    case "$1" in
        --root) ROOT_PASS=1; NONROOT_PASS=0 ;;
        --nonroot) ROOT_PASS=0; NONROOT_PASS=1 ;;
        --coverage) COVERAGE=1; ROOT_PASS=0; NONROOT_PASS=1 ;;
        --no-build) BUILD=0 ;;
        --min-coverage) shift; MIN_COVERAGE="$1" ;;
        --) shift; TEST_ARGS+=("$@"); break ;;
        -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) TEST_ARGS+=("$1") ;;
    esac
    shift
done

need_docker() {
    command -v docker >/dev/null || { echo "docker is required" >&2; exit 1; }
    docker info >/dev/null 2>&1 || { echo "cannot talk to the docker daemon" >&2; exit 1; }
}

run_pass() {
    local name="$1"; shift
    echo
    echo "== e2e: $name =="
    docker run --rm \
        --privileged \
        --user "$E2E_UID" \
        -e HOME=/tmp \
        "$@" \
        "$IMAGE" "${TEST_ARGS[@]}"
}

need_docker

if [ "$BUILD" = 1 ] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "== building $IMAGE =="
    docker build -f e2e/Dockerfile -t "$IMAGE" .
fi

if [ "$ROOT_PASS" = 1 ]; then
    E2E_UID=0 run_pass "root (real CAP_NET_ADMIN)"
fi

if [ "$NONROOT_PASS" = 1 ]; then
    E2E_UID=1000:1000 run_pass "uid 1000 (rootless user namespace)"
fi

if [ "$COVERAGE" = 1 ]; then
    echo
    echo "== e2e: coverage =="
    if [ "$BUILD" = 1 ] || ! docker image inspect "$COVERAGE_IMAGE" >/dev/null 2>&1; then
        echo "== building $COVERAGE_IMAGE (instrumented) =="
        docker build -f e2e/Dockerfile --build-arg INSTRUMENT=1 -t "$COVERAGE_IMAGE" .
    fi

    # A named volume instead of a bind mount: it lives on the daemon side, so it
    # works with a remote or sandboxed docker as well.
    volume="${E2E_COVERAGE_VOLUME:-adhoc-e2e-coverage}"
    docker volume create "$volume" >/dev/null
    docker run --rm -v "$volume:/coverage" --entrypoint /bin/sh "$COVERAGE_IMAGE" \
        -c 'rm -rf /coverage/* 2>/dev/null; mkdir -p /coverage/root /coverage/user; chmod 0777 /coverage /coverage/root /coverage/user'

    # Root and rootless runs write into separate directories (both containers
    # start at pid 1, so the profile names would otherwise collide) and every
    # process that inherits LLVM_PROFILE_FILE contributes, including the apps
    # adhoc spawns.
    for pass in root:0 user:1000:1000; do
        label="${pass%%:*}"
        uid="${pass#*:}"
        echo "== e2e coverage: uid $uid =="
        docker run --rm --privileged --user "$uid" -e HOME=/tmp \
            -e LLVM_PROFILE_FILE="/coverage/$label/adhoc-%p-%m.profraw" \
            -v "$volume:/coverage" \
            "$COVERAGE_IMAGE" "${TEST_ARGS[@]}"
    done

    mkdir -p e2e/coverage
    docker run --rm --privileged \
        --entrypoint /usr/local/bin/coverage.sh \
        -e COVERAGE_DIR=/coverage \
        -e MIN_COVERAGE="$MIN_COVERAGE" \
        -v "$volume:/coverage" \
        "$COVERAGE_IMAGE" | tee e2e/coverage/report.txt
fi

echo
echo "== e2e: done =="
