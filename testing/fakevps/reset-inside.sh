#!/bin/bash
# Back to the clean seeded state: same files, freshly started containers. Runs as root inside fakevps.
set -uo pipefail
BASE=/home/kdresdell/minipass_env
CLEAN=/var/cache/fakevps/clean
ME=kdresdell

systemctl stop minipass-web 2>/dev/null || true
for name in $(docker ps -aq); do docker rm -f "$name" >/dev/null 2>&1; done
docker network rm minipass_env_proxy >/dev/null 2>&1 || true
pkill -u "$ME" -f '[m]ptool' 2>/dev/null || true
rm -rf "$BASE" /home/kdresdell/mptool-undo /home/kdresdell/.mailpass /tmp/mptool.* /etc/systemd/system/minipass-web.service
crontab -u "$ME" -r 2>/dev/null || true

tar --numeric-owner -xpf "$CLEAN/home.tar" -C /home/kdresdell
cp "$CLEAN/minipass-web.service" /etc/systemd/system/
crontab -u "$ME" "$CLEAN/crontab"
systemctl daemon-reload
systemctl enable -q minipass-web
systemctl restart minipass-web

cd "$BASE" || exit 1
sudo -u "$ME" -H docker-compose up -d >/dev/null 2>&1 || { echo "reset: main containers did not start"; exit 1; }
for d in deployed/*/; do (cd "$d" && sudo -u "$ME" -H docker-compose up -d >/dev/null 2>&1) || { echo "reset: $d did not start"; exit 1; }; done

fails=""
ready() {
    docker exec mailserver doveadm user alice@minipass.me >/dev/null 2>&1 || return 1
    docker exec mailserver bash -c 'exec 3<>/dev/tcp/127.0.0.1/25' 2>/dev/null || return 1
    docker exec mailserver bash -c 'exec 3<>/dev/tcp/127.0.0.1/143' 2>/dev/null || return 1
    for c in kdc testb; do
        curl -sk --max-time 5 --resolve "$c.minipass.me:443:127.0.0.1" "https://$c.minipass.me/login" | grep -q 'type="password"' || return 1
    done
    [ "$(curl -sk --max-time 5 --resolve minipass.me:443:127.0.0.1 -o /dev/null -w '%{http_code}' https://minipass.me/)" = 200 ] || return 1
}
for _ in $(seq 150); do ready && exit 0; sleep 2; done
echo "reset: the fake VPS did not come back up"; exit 1
