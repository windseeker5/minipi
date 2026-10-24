#!/usr/bin/env bash
# update.sh — Get the latest backup tool from GitHub, build it and install it
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

cd "$REPO/backup"
cargo build --release --locked "${JOBS[@]}"
install -Dm755 target/release/mpbackup "$HOME/.local/bin/mpbackup"
install -Dm644 -t "$HOME/.config/systemd/user" systemd/*.service systemd/*.timer
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
systemctl --user daemon-reload 2>/dev/null || true
# The screen keeps running the old program until restarted.
systemctl --user try-restart mpbackup-screen.service 2>/dev/null || true
print_success "mpbackup installed in ~/.local/bin"
