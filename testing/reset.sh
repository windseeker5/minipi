#!/bin/bash
# Put the fake VPS back to its clean seeded state. With --rebuild: throw away the container and start again from nothing
# (only Docker's image store and the pip cache are kept, so the rebuild doesn't download everything again).
set -euo pipefail
cd "$(dirname "$0")"
if [ "${1:-}" = "--rebuild" ]; then
    docker volume rm -f fakevps-state >/dev/null 2>&1 || true
    ./build.sh
    docker exec fakevps rm -rf /var/cache/fakevps/clean /var/cache/fakevps/src
    ./seed.sh
else
    docker exec fakevps /usr/local/sbin/fakevps-reset
fi
