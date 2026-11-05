#!/bin/bash
# Build and start the fake VPS, then prove the firewall holds. Starts no app.
set -euo pipefail
cd "$(dirname "$0")"
STATE="$PWD/.state"
mkdir -p "$STATE/ssh" "$STATE/home"
chmod 700 "$STATE"

[ -f "$STATE/ssh/fakevps_key" ] || ssh-keygen -q -t ed25519 -N '' -C fakevps-test -f "$STATE/ssh/fakevps_key"

# Made fresh here and kept out of git: the fake server's logins are random, never written in the code.
PASSWORDS="$STATE/test-passwords"
if [ ! -f "$PASSWORDS" ]; then
    (umask 077; for who in alice alice2 bob kdc_app testb_app mailpass; do echo "$who $(openssl rand -hex 16)"; done > "$PASSWORDS")
fi
CHECK_LOGIN=$(awk '$1 == "alice" {print $2}' "$PASSWORDS")

# The settings mptool uses for the fake VPS. Never the real server, and real_vps_ok stays out.
cat > "$STATE/config.toml" <<EOT
server = "kdresdell@127.0.0.1"
port = 2223
remote_base = "/home/kdresdell/minipass_env"
backup_root = "$STATE/backups"
discord_webhook = ""
restore_allowed = ["kdc", "testb"]
restore_enabled = true
repo_env = "/srv/git/minipass_env.git"
repo_app = "/srv/git/dpm.git"
mail_check_user = "alice@minipass.me"
mail_check_password = "$CHECK_LOGIN"
ssh_options = ["-F", "/dev/null", "-i", "$STATE/ssh/fakevps_key", "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=accept-new", "-o", "UserKnownHostsFile=$STATE/ssh/known_hosts"]
EOT

docker build -q -t fakevps:latest fakevps >/dev/null
docker network inspect fakevps-net >/dev/null 2>&1 || docker network create fakevps-net >/dev/null
docker rm -f fakevps >/dev/null 2>&1 || true
docker run -d --name fakevps --hostname fakevps --privileged --cgroupns=private \
    --network fakevps-net -p 127.0.0.1:2223:2222 \
    --dns 1.1.1.1 --dns 9.9.9.9 --tmpfs /run --tmpfs /run/lock \
    --memory 10g --cpus 8 \
    -v fakevps-docker:/var/lib/docker -v fakevps-containerd:/var/lib/containerd -v fakevps-cache:/var/cache/fakevps \
    fakevps:latest >/dev/null

echo "Waiting for the fake VPS to boot..."
for _ in $(seq 60); do
    state=$(docker exec fakevps systemctl is-system-running 2>/dev/null || true)
    [ "$state" = running ] || [ "$state" = degraded ] && break
    sleep 1
done
docker exec fakevps systemctl is-active fakevps-firewall docker ssh
docker exec fakevps chown kdresdell:kdresdell /var/cache/fakevps
docker cp -q "$STATE/ssh/fakevps_key.pub" fakevps:/home/kdresdell/.ssh/authorized_keys
docker exec fakevps sh -c 'chown kdresdell:kdresdell /home/kdresdell/.ssh/authorized_keys; chmod 600 /home/kdresdell/.ssh/authorized_keys'

rm -f "$STATE/ssh/known_hosts"
ssh -F /dev/null -i "$STATE/ssh/fakevps_key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new \
    -o UserKnownHostsFile="$STATE/ssh/known_hosts" -p 2223 kdresdell@127.0.0.1 'echo "ssh works as $(whoami) on $(hostname)"'

docker cp -q fakevps/firewall-test.sh fakevps:/usr/local/sbin/fakevps-firewall-test
docker exec fakevps bash /usr/local/sbin/fakevps-firewall-test
echo "Fake VPS is up. Nothing is running in it yet."
