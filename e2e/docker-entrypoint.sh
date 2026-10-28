#!/bin/sh
# Entrypoint for the adhoc e2e image.
#
# Does the two bits of container setup the suite needs and then runs the test
# binary with whatever arguments the caller passed:
#
#   docker run --rm --privileged adhoc-e2e                 # root pass
#   docker run --rm --privileged --user 1000:1000 adhoc-e2e # rootless pass
#
# 1. /dev/net/tun is a tmpfs entry in every container, so an image cannot carry
#    the device node. pasta needs it for outbound-network tests; create it when
#    we are allowed to (root + privileged) and let those tests skip otherwise.
# 2. Point LLVM_PROFILE_FILE somewhere writable when coverage is enabled.

set -eu

if [ ! -e /dev/net/tun ]; then
    if [ "$(id -u)" = "0" ]; then
        mkdir -p /dev/net
        # mknod fails without CAP_MKNOD; that is fine, the pasta test skips.
        mknod /dev/net/tun c 10 200 2>/dev/null || true
        chmod 0666 /dev/net/tun 2>/dev/null || true
    fi
fi

if [ -n "${LLVM_PROFILE_FILE:-}" ]; then
    mkdir -p "$(dirname "$LLVM_PROFILE_FILE")" 2>/dev/null || true
fi

echo "== adhoc e2e: uid=$(id -u) bin=${ADHOC_BIN:-unset} =="
if [ -e /dev/net/tun ]; then
    echo "== /dev/net/tun present: outbound-network tests will run =="
else
    echo "== /dev/net/tun missing: outbound-network tests will skip =="
fi

exec /usr/local/bin/adhoc-e2e "$@"
