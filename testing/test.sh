#!/bin/bash
# The whole finish line, on the fake VPS: release build without warnings, cargo test with no network, all restore tests.
# Add --rebuild to start from a brand new fake VPS. Names after that run only those tests.
set -uo pipefail
cd "$(dirname "$0")"
if [ "${1:-}" = "--rebuild" ]; then
    shift
    echo "== clean rebuild"
    ./reset.sh --rebuild >/dev/null || { echo "the rebuild failed"; exit 1; }
fi
echo "== cargo build --release"
touch ../mptool/src/main.rs
out=$(cd ../mptool && cargo build --release 2>&1) || { echo "$out"; exit 1; }
if echo "$out" | grep -q '^warning'; then echo "$out"; echo "there are warnings"; exit 1; fi
echo "no warnings"
echo "== cargo test, no network"
(cd ../mptool && unshare -rn cargo test --release --offline 2>&1 | tail -3 | grep 'test result: ok') || { echo "cargo test failed"; exit 1; }
echo "== restore tests"
./run_tests.py "$@"
