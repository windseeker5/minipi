#!/bin/bash
# Fill the fake VPS with fake content, then save it as the clean state that reset.sh goes back to.
set -euo pipefail
cd "$(dirname "$0")"
./make-source.sh >/dev/null
docker exec fakevps mkdir -p /var/cache/fakevps/src
docker cp -q .state/src/env.tar fakevps:/var/cache/fakevps/src/minipass_env.tar
docker cp -q .state/src/dpm.tar fakevps:/var/cache/fakevps/src/dpm.tar
docker exec fakevps chown -R kdresdell /var/cache/fakevps/src
docker cp -q fakevps/seed-inside.sh fakevps:/usr/local/sbin/fakevps-seed
docker cp -q fakevps/reset-inside.sh fakevps:/usr/local/sbin/fakevps-reset
docker cp -q fakevps/snapshot.py fakevps:/usr/local/sbin/fakevps-snapshot
docker cp -q fakevps/check-dkim.py fakevps:/usr/local/sbin/fakevps-check-dkim
docker exec fakevps chmod 755 /usr/local/sbin/fakevps-reset /usr/local/sbin/fakevps-snapshot /usr/local/sbin/fakevps-check-dkim
docker exec fakevps bash /usr/local/sbin/fakevps-seed
