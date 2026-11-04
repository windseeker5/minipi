#!/bin/bash
# Copy the code for the fake VPS. git archive only: the working folders hold real .env files and databases.
# Tracked real data (forwards with real addresses, the mail config, customer vhosts) stays out too.
set -euo pipefail
cd "$(dirname "$0")"
DEV=$(cd ../.. && pwd)
OUT="$PWD/.state/src"
rm -rf "$OUT"; mkdir -p "$OUT/env" "$OUT/dpm"

git -C "$DEV/minipass_env" archive HEAD .gitignore docker-compose.yml nginx MinipassWebSite | tar x -C "$OUT/env"
git -C "$DEV/minipass_env/app" archive HEAD | tar x -C "$OUT/dpm"

cd "$OUT/env/MinipassWebSite"
rm -rf config .mcp.json .agents .claude CLAUDE.md DESIGN.md PRODUCT.md design.html guide-beta-testeur.md \
       docs tests scripts secrets Marketing static/video
cd "$OUT/dpm"
rm -rf docs uattool pytest tests .agents .claude .github CLAUDE.md AGENTS.md test test_*.py

# Rule 5: nothing asks Let's Encrypt for anything, and the companion has no way to reach it.
cd "$OUT/env"
sed -i '/LETSENCRYPT_/d' docker-compose.yml
sed -i 's|^      - NGINX_PROXY_CONTAINER=nginx-proxy|&\n      - ACME_CA_URI=https://127.0.0.1:9/directory|' docker-compose.yml

# Stop here if anything that looks like a real secret came along.
cd "$OUT"
PATTERN='(sk|rk|pk)_live_[A-Za-z0-9]{16}|whsec_[A-Za-z0-9]{20}|AIza[0-9A-Za-z_-]{30}|gsk_[A-Za-z0-9]{20}|discord(app)?\.com/api/webhooks/[0-9]|BEGIN [A-Z ]*PRIVATE KEY|ghp_[A-Za-z0-9]{20}|xox[bp]-'
if hits=$(grep -rIlE "$PATTERN" . 2>/dev/null); then
    echo "Possible real secrets in the copied code, stopping:"; echo "$hits"; exit 1
fi
if find . \( -name '*.db' -o -name '*.sqlite*' -o -name '.env' -o -name '.env.*' -o -name '*.pem' -o -name '*.private' \) | grep .; then
    echo "Data files in the copied code, stopping."; exit 1
fi
if grep -nE 'LETSENCRYPT_(HOST|EMAIL)' env/docker-compose.yml; then echo "LETSENCRYPT left in compose"; exit 1; fi
tar cf "$OUT/env.tar" -C "$OUT/env" . ; tar cf "$OUT/dpm.tar" -C "$OUT/dpm" .
du -sh "$OUT/env" "$OUT/dpm"
echo "Code copied and clean."
