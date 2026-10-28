#!/bin/bash
# Merges the .profraw files written during an instrumented e2e run and prints a
# coverage report for the launcher and the test binary.
#
# Runs *inside* the e2e image (see e2e/run.sh --coverage); the image has
# llvm-tools installed and the sources in /src.
#
#   COVERAGE_DIR   where the .profraw files live (default /coverage)
#   ADHOC_BIN      instrumented binary to report on (default /usr/local/bin/adhoc)
#   MIN_COVERAGE   fail below this percentage of line coverage (default 70)

set -euo pipefail

dir="${COVERAGE_DIR:-/coverage}"
min="${MIN_COVERAGE:-70}"
llvm_bin="$(dirname "$(find "$(rustc --print sysroot)" -name llvm-profdata -type f | head -1)")"
profdata="$dir/merged.profdata"

mapfile -t profiles < <(find "$dir" -type f -name '*.profraw' | sort)
if [ ${#profiles[@]} -eq 0 ]; then
    echo "coverage: no .profraw files in $dir" >&2
    exit 1
fi
echo "coverage: merging ${#profiles[@]} profile(s)"

"$llvm_bin/llvm-profdata" merge -sparse "${profiles[@]}" -o "$profdata"

# Only the launcher: the test binary embeds the same crate with a different
# optimisation level, which makes llvm-cov warn about mismatched functions and
# contributes nothing the release build does not already cover.
"$llvm_bin/llvm-cov" report "${ADHOC_BIN:-/usr/local/bin/adhoc}" \
    -instr-profile="$profdata" \
    --ignore-filename-regex='(/cargo/registry/|/rustlib/|/rustc/)' \
    | tee "$dir/report.txt"

total="$(awk '/^TOTAL/ {gsub("%", "", $10); print $10}' "$dir/report.txt")"
echo
echo "coverage: line coverage ${total}% (minimum ${min}%)"
awk -v total="$total" -v min="$min" 'BEGIN { exit !(total + 0 >= min + 0) }' || {
    echo "coverage below threshold" >&2
    exit 1
}
