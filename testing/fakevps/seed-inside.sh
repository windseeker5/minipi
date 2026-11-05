#!/bin/bash
# Fill the fake VPS with fake content, the way the real server is laid out. Runs as root inside fakevps.
# Every password, key, address and message here is made up.
set -euo pipefail
BASE=/home/kdresdell/minipass_env
ME=kdresdell
DMS=ghcr.io/docker-mailserver/docker-mailserver:latest
as_me() { sudo -u "$ME" -H "$@"; }
log() { echo "[seed] $*"; }
# Logins come from a file made by build.sh. Secrets the apps need are made fresh at each seed.
pw() { awk -v who="$1" '$1 == who {print $2}' /var/cache/fakevps/test-passwords; }
INTERNAL_SECRET=$(openssl rand -hex 16)

# ── GitHub stand-in: bare repos, one commit each ──
for repo in minipass_env dpm; do
    [ -d "/srv/git/$repo.git" ] && continue
    log "code repo $repo"
    src=/var/cache/fakevps/src/$repo.tar
    tmp=$(mktemp -d); chown "$ME" "$tmp"
    as_me tar xf "$src" -C "$tmp"
    as_me git init -q --bare -b main "/srv/git/$repo.git"
    as_me git -C "$tmp" init -q -b main
    as_me git -C "$tmp" add -A
    as_me git -C "$tmp" -c user.name=fakevps -c user.email=fakevps@example.invalid commit -q -m "fake VPS code snapshot"
    as_me git -C "$tmp" push -q "/srv/git/$repo.git" main
    rm -rf "$tmp"
done

# ── Start clean ── (Docker's store survives a rebuild, so its old containers would still be there.)
docker ps -aq | xargs -r docker rm -f >/dev/null
docker network rm minipass_env_proxy >/dev/null 2>&1 || true
for d in "$BASE"/deployed/*/ "$BASE"/; do
    [ -f "$d/docker-compose.yml" ] && (cd "$d" && docker-compose down -v --remove-orphans >/dev/null 2>&1 || true)
done
systemctl stop minipass-web 2>/dev/null || true
rm -rf "$BASE" /home/kdresdell/mptool-undo /home/kdresdell/.mailpass /etc/systemd/system/minipass-web.service

log "server files"
as_me git clone -q -b main /srv/git/minipass_env.git "$BASE"
cd "$BASE"
as_me mkdir -p config/opendkim/keys/minipass.me vhost.d html certs acme maildata mailstate deployed email_monitoring

as_me tee .env >/dev/null <<EOT
# fake
STRIPE_SECRET_KEY=sk_test_fake
GOOGLE_MAPS_API_KEY=fake
INTERNAL_API_SECRET=$INTERNAL_SECRET
EOT
as_me tee mailserver.env >/dev/null <<EOT
OVERRIDE_HOSTNAME=mail.minipass.me
SSL_TYPE=letsencrypt
ENABLE_OPENDKIM=1
ENABLE_OPENDMARC=0
ENABLE_POLICYD_SPF=0
ENABLE_AMAVIS=0
ENABLE_SPAMASSASSIN=0
ENABLE_CLAMAV=0
ENABLE_FAIL2BAN=0
ENABLE_POSTGREY=0
ENABLE_MANAGESIEVE=1
ENABLE_QUOTAS=0
ENABLE_RSPAMD=0
ONE_DIR=1
PERMIT_DOCKER=connected-networks
POSTFIX_INET_PROTOCOLS=ipv4
DOVECOT_INET_PROTOCOLS=ipv4
EOT
pw mailpass | as_me tee /home/kdresdell/.mailpass >/dev/null
chmod 600 /home/kdresdell/.mailpass
as_me tee /tmp/crontab.fake >/dev/null <<EOT
# fake crontab: does nothing
17 4 * * * true
EOT
as_me crontab /tmp/crontab.fake; rm -f /tmp/crontab.fake

for c in kdc testb; do
    as_me tee "vhost.d/$c.minipass.me_location" >/dev/null <<EOT
# ${c^^} — standard customer
client_max_body_size 20M;
EOT
done

log "certificates (self-signed)"
hosts="minipass.me www.minipass.me mail.minipass.me kdc.minipass.me testb.minipass.me"
for h in $hosts; do
    as_me mkdir -p "certs/$h"
    as_me openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=$h" \
        -keyout "certs/$h/key.pem" -out "certs/$h/fullchain.pem" 2>/dev/null
    as_me ln -s "./$h/fullchain.pem" "certs/$h.crt"
    as_me ln -s "./$h/key.pem" "certs/$h.key"
done

log "mail settings"
docker image inspect "$DMS" >/dev/null 2>&1 || docker pull -q "$DMS" >/dev/null
hash_pw() { docker run --rm --entrypoint doveadm "$DMS" pw -s SHA512-CRYPT -p "$1"; }
: > /tmp/accounts.fake
for who in alice bob kdc_app testb_app; do
    echo "$who@minipass.me|$(hash_pw "$(pw "$who")")" >> /tmp/accounts.fake
done
as_me cp /tmp/accounts.fake config/postfix-accounts.cf; rm -f /tmp/accounts.fake
as_me tee config/postfix-virtual.cf >/dev/null <<EOT
info@minipass.me alice@minipass.me
sales@minipass.me bob@minipass.me
EOT
as_me mkdir -p "config/user-patches/kdc_app@minipass.me/sieve"
as_me tee "config/user-patches/kdc_app@minipass.me/sieve/forward.sieve" >/dev/null <<EOT
require ["fileinto", "copy"];
redirect :copy "alice@minipass.me";
EOT
as_me openssl genrsa -out config/opendkim/keys/minipass.me/mail.private 2048 2>/dev/null
pub=$(openssl rsa -in config/opendkim/keys/minipass.me/mail.private -pubout -outform DER 2>/dev/null | base64 -w0)
as_me tee config/opendkim/keys/minipass.me/mail.txt >/dev/null <<EOT
mail._domainkey IN TXT ( "v=DKIM1; h=sha256; k=rsa; p=$pub" )
EOT
as_me tee config/opendkim/KeyTable >/dev/null <<EOT
mail._domainkey.minipass.me minipass.me:mail:/etc/opendkim/keys/minipass.me/mail.private
EOT
as_me tee config/opendkim/SigningTable >/dev/null <<EOT
*@minipass.me mail._domainkey.minipass.me
EOT
as_me tee config/opendkim/TrustedHosts >/dev/null <<EOT
127.0.0.1
localhost
EOT

log "starting the main containers"
as_me docker-compose up -d 2>&1 | tail -3
as_me bash -c 'for i in $(seq 120); do docker exec mailserver doveadm user alice@minipass.me >/dev/null 2>&1 && docker exec mailserver bash -c "exec 3<>/dev/tcp/127.0.0.1/25" 2>/dev/null && exit 0; sleep 2; done; docker logs --tail 40 mailserver; exit 1'

log "messages"
send() { # from to subject
    printf 'From: %s\nTo: %s\nSubject: %s\nMessage-ID: <%s@minipass.me>\n\nFake message: %s\n' \
        "$1" "$2" "$3" "$(openssl rand -hex 8)" "$3" | docker exec -i mailserver sendmail -f "$1" "$2"
}
for n in 1 2 3 4; do send bob@minipass.me alice@minipass.me "alice inbox $n"; done
for n in 1 2 3; do send alice@minipass.me bob@minipass.me "bob inbox $n"; done
send alice@minipass.me info@minipass.me "to the info alias"
send alice@minipass.me kdc_app@minipass.me "for kdc app"
send alice@minipass.me testb_app@minipass.me "for testb app"
for i in $(seq 40); do
    n=$(docker exec mailserver sh -c 'find /var/mail -type f \( -path "*/new/*" -o -path "*/cur/*" \) | wc -l')
    [ "$n" -ge 10 ] && [ -z "$(docker exec mailserver postqueue -j)" ] && break
    sleep 1
done
# Some read, some not: reading moves a message to cur/ and renames it.
docker exec mailserver doveadm flags add -u alice@minipass.me '\Seen' mailbox INBOX SUBJECT "alice inbox 1"
docker exec mailserver doveadm flags add -u alice@minipass.me '\Seen' mailbox INBOX SUBJECT "alice inbox 2"
docker exec mailserver doveadm flags add -u bob@minipass.me '\Seen' mailbox INBOX SUBJECT "bob inbox 1"
docker exec mailserver doveadm mailbox create -u alice@minipass.me Archive
docker exec mailserver doveadm move -u alice@minipass.me Archive mailbox INBOX SUBJECT "alice inbox 2"

log "forward (installed the way the website does it)"
home=/var/mail/minipass.me/kdc_app/home
docker exec mailserver mkdir -p "$home/sieve"
docker cp -q "config/user-patches/kdc_app@minipass.me/sieve/forward.sieve" "mailserver:$home/sieve/forward.sieve"
docker exec mailserver chown -R docker:docker "$home"
docker exec mailserver doveadm sieve put -u kdc_app@minipass.me forward "$home/sieve/forward.sieve"
docker exec mailserver doveadm sieve activate -u kdc_app@minipass.me forward

log "website"
cd "$BASE/MinipassWebSite"
as_me tee .env >/dev/null <<EOT
SECRET_KEY=$(openssl rand -hex 16)
STRIPE_SECRET_KEY=sk_test_fake
STRIPE_PUBLISHABLE_KEY=pk_test_fake
STRIPE_WEBHOOK_SECRET=fake
INTERNAL_API_SECRET=$INTERNAL_SECRET
EOT
as_me python3 -m venv venv
as_me venv/bin/pip install -q -r requirements.txt 2>&1 | tail -2
as_me venv/bin/python - <<'EOT'
import sqlite3
from utils.customer_helpers import init_customers_db
init_customers_db()
c = sqlite3.connect("customers.db")
for name in ("kdc", "testb"):
    c.execute("INSERT INTO customers (email, subdomain, app_name, plan, admin_password, port, deployed) VALUES (?,?,?,?,?,?,1)",
              (f"{name}@example.invalid", name, name, "basic", "fake", 8889))
c.commit()
EOT
as_me sqlite3 "$BASE/email_monitoring/monitoring.db" \
    "CREATE TABLE mailbox_sizes (day TEXT, mailbox TEXT, size INTEGER); INSERT INTO mailbox_sizes VALUES ('2026-11-01','alice',1000),('2026-11-02','alice',1100);"
cat > /etc/systemd/system/minipass-web.service <<EOT
[Unit]
Description=Minipass website (fake VPS)
After=network.target

[Service]
User=$ME
WorkingDirectory=$BASE/MinipassWebSite
ExecStart=$BASE/MinipassWebSite/venv/bin/python app.py
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
EOT
systemctl daemon-reload
systemctl enable -q minipass-web
systemctl restart minipass-web

for c in kdc testb; do
    log "customer $c"
    d="$BASE/deployed/$c"
    as_me mkdir -p "$d"
    as_me git clone -q -b main /srv/git/dpm.git "$d/app"
    as_me mkdir -p "$d/app/instance" "$d/app/static/uploads/avatars"
    as_me tee "$d/app/.env" >/dev/null <<EOT
# fake
FLASK_SECRET_KEY=$(openssl rand -hex 16)
STRIPE_SECRET_KEY=
GOOGLE_MAPS_API_KEY=
GOOGLE_AI_API_KEY=
GROQ_API_KEY=
UNSPLASH_ACCESS_KEY=
CHATBOT_ENABLE_GEMINI=false
CHATBOT_ENABLE_GROQ=false
CHATBOT_ENABLE_OLLAMA=false
APP_SUBDOMAIN=$c
MINIPASS_SITE_URL=http://127.0.0.1:9
INTERNAL_API_SECRET=$INTERNAL_SECRET
EOT
    as_me tee "$d/docker-compose.yml" >/dev/null <<EOT
version: '3.8'

services:
  flask-app:
    container_name: minipass_$c
    build:
      context: ./app

    env_file:
      - ./app/.env

    volumes:
      - ./app:/app
      - ./app/instance:/app/instance
      - ./app/static/uploads:/app/static/uploads
    environment:
      - FLASK_ENV=dev
      - SITE_URL=https://$c.minipass.me

      - VIRTUAL_HOST=$c.minipass.me
      - VIRTUAL_PORT=8889

    restart: unless-stopped
    networks:
      - proxy

networks:
  proxy:
    name: minipass_env_proxy
    external: true
EOT
    (cd "$d" && as_me docker-compose build -q 2>&1 | tail -2)
    (cd "$d" && as_me docker-compose run --rm --no-deps -e FLASK_APP=app.py flask-app flask db upgrade 2>&1 | tail -1)
    # Same second step as the real deploy: it brings the tables up to the current code.
    (cd "$d" && as_me docker-compose run --rm --no-deps -e DATABASE_PATH=/app/instance/minipass.db flask-app \
        python migrations/upgrade_production_database.py 2>&1 | tail -1)
    as_me sqlite3 "$d/app/instance/minipass.db" \
        "INSERT INTO admin (email, password_hash) VALUES ('admin-$c@example.invalid','fake-not-a-real-hash');"
    as_me sqlite3 "$d/app/instance/minipass.db" \
        "INSERT INTO user (name, email, email_opt_out) VALUES ('Fake Person $c','person-$c@example.invalid',0),('Second Person $c','second-$c@example.invalid',0);"
    # A small real photo, different for each customer.
    as_me python3 - "$d/app/static/uploads/photo-$c.png" "$c" <<'EOT'
import struct, sys, zlib
out, name = sys.argv[1:]
shade = sum(map(ord, name)) % 256
raw = b"".join(b"\x00" + bytes([shade]) * 16 for _ in range(16))
def chunk(t, d):
    return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
open(out, "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 16, 16, 8, 0, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
EOT
    (cd "$d" && as_me docker-compose up -d 2>&1 | tail -1)
done

# For the tests: checks a DKIM signature against a key, without any DNS. Lives outside the server's files.
[ -x /var/cache/fakevps/dkim-venv/bin/python ] || {
    log "signature checker"
    as_me python3 -m venv /var/cache/fakevps/dkim-venv
    as_me /var/cache/fakevps/dkim-venv/bin/pip install -q dkimpy 2>&1 | tail -1
}

# ── Save this as the clean state reset.sh goes back to ──
log "saving the clean state"
cd "$BASE"
for d in deployed/*/ ./; do (cd "$d" && as_me docker-compose stop -t 5 >/dev/null 2>&1); done
systemctl stop minipass-web
mkdir -p /var/cache/fakevps/clean
tar --numeric-owner -cf /var/cache/fakevps/clean/home.tar -C /home/kdresdell minipass_env .mailpass
cp /etc/systemd/system/minipass-web.service /var/cache/fakevps/clean/
crontab -u "$ME" -l > /var/cache/fakevps/clean/crontab
/usr/local/sbin/fakevps-reset
log "done"
