#!/bin/bash
# A shell on the fake VPS, as kdresdell, with the test key. Only ever talks to 127.0.0.1:2223.
cd "$(dirname "$0")"
exec ssh -F /dev/null -i .state/ssh/fakevps_key -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new \
    -o UserKnownHostsFile=.state/ssh/known_hosts -p 2223 kdresdell@127.0.0.1 "$@"
