#!/usr/bin/env bash
# update.sh — Get the latest mptool from GitHub, build it and install it
# Run on the Pi as your user (not root):  ~/minipi/pi/update.sh
set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; BOLD='\033[1m'; NC='\033[0m'
print_success(){ printf "${GREEN}${BOLD}[✓]${NC} %s\n" "$1"; }
print_error()  { printf "${RED}${BOLD}[✗]${NC} %s\n" "$1" >&2; exit 1; }

[[ $EUID -ne 0 ]] || print_error "Run as your user, not root."
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

git -C "$REPO" pull -q --ff-only || print_error "Could not pull from GitHub (local changes in $REPO?)"
print_success "Code up to date ($(git -C "$REPO" log -1 --format='%h %s'))"

# Building with all 4 cores can run a 2 GB Pi out of memory.
MEM_KB="$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
JOBS=()
[[ "$MEM_KB" -lt 3000000 ]] && JOBS=(-j 2)

cd "$REPO/mptool"
cargo build --release --locked "${JOBS[@]}"
install -Dm755 target/release/mptool "$HOME/.local/bin/mptool"
install -Dm644 -t "$HOME/.config/systemd/user" systemd/*.service systemd/*.timer
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
systemctl --user daemon-reload

# Once, after the rename from mpbackup: same settings and timers under the new names.
if [[ -d "$HOME/.config/mpbackup" && ! -e "$HOME/.config/mptool" ]]; then
    mv "$HOME/.config/mpbackup" "$HOME/.config/mptool"
fi
for pair in mpbackup.timer:mptool-backup.timer mpbackup-screen.service:mptool-screen.service; do
    if systemctl --user is-enabled -q "${pair%%:*}" 2>/dev/null; then
        systemctl --user disable --now "${pair%%:*}"
        systemctl --user enable --now "${pair##*:}"
    fi
done
USER_UNITS="$HOME/.config/systemd/user"
rm -f "$USER_UNITS/mpbackup.service" "$USER_UNITS/mpbackup.timer" "$USER_UNITS/mpbackup-screen.service" \
    "$HOME/.local/bin/mpbackup"
systemctl --user daemon-reload

# Site checks every 15 minutes, and the screen restarted onto the new program.
systemctl --user enable --now mptool-monitor.timer
systemctl --user try-restart mptool-screen.service
print_success "mptool installed in ~/.local/bin (site checks every 15 minutes)"
