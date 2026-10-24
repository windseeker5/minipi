#!/usr/bin/env bash
# install.sh — Set up the Mini Pi from your LAPTOP, over the network
# Run after flash.sh, once the Pi is plugged in and powered on:  ./install.sh  [pi-address]
#
# It finds the Pi, runs pi/setup.sh on it (Arch update, your user, the backup tool),
# then copies your backup settings and lets the Pi log in to the Minipass server.
set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
_STEP_NUM=0
print_step()   { _STEP_NUM=$((_STEP_NUM+1)); printf "\n${BLUE}${BOLD}[Step %d]${NC} %s\n" "$_STEP_NUM" "$1"; }
print_success(){ printf "${GREEN}${BOLD}[✓]${NC} %s\n" "$1"; }
print_warn()   { printf "${YELLOW}${BOLD}[!]${NC} %s\n" "$1"; }
print_error()  { printf "${RED}${BOLD}[✗]${NC} %s\n" "$1" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAPTOP_CONFIG="$HOME/.config/mpbackup/config.toml"
PI_BACKUP_ROOT="/mnt/backup/minipass"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout=5)
# Raspberry Pi network cards start with one of these (Raspberry Pi Foundation / Trading).
PI_MACS='^(b8:27:eb|dc:a6:32|e4:5f:01|d8:3a:dd|28:cd:c1|2c:cf:67|88:a2:9e)'

# toml_get KEY → the value of `KEY = ...` in the laptop's backup config, without quotes.
toml_get() {
    sed -nE "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\"?([^\"#]*)\"?.*/\1/p" "$LAPTOP_CONFIG" | head -1 | sed 's/[[:space:]]*$//'
}

ssh_port_open() { timeout 2 bash -c ">/dev/tcp/$1/22" 2>/dev/null; }

# Ping every address on the local network, then list the ones with a Raspberry Pi network card.
scan_for_pi() {
    local dev src prefix
    read -r dev src < <(ip -4 route get 1.1.1.1 | sed -nE 's/.* dev ([^ ]+).* src ([^ ]+).*/\1 \2/p')
    [[ -n "$src" ]] || return 0
    prefix="${src%.*}"
    for i in $(seq 1 254); do ping -c1 -W1 "${prefix}.${i}" &>/dev/null & done
    wait
    ip neigh show dev "$dev" | awk -v re="$PI_MACS" 'tolower($3) ~ re { print $1 }'
}

# ── Step 1: Your backup settings ─────────────────────────────────────────────
print_step "Reading your backup settings"
HAVE_CONFIG=0
if [[ -f "$LAPTOP_CONFIG" ]]; then
    HAVE_CONFIG=1
    SERVER="$(toml_get server)"
    PORT="$(toml_get port)"
    print_success "Found $LAPTOP_CONFIG (server ${SERVER}, port ${PORT})"
else
    print_warn "No $LAPTOP_CONFIG on this laptop."
    print_warn "The Pi gets the example settings; fill them in later on the Pi."
fi

# ── Step 2: Find the Pi ──────────────────────────────────────────────────────
print_step "Looking for the Pi on your network"
PI="${1:-}"
if [[ -z "$PI" ]]; then
    for name in minipi minipi.lan minipi.home minipi.local; do
        if getent hosts "$name" &>/dev/null && ssh_port_open "$name"; then PI="$name"; break; fi
    done
fi
if [[ -z "$PI" ]]; then
    printf "Scanning the network (takes a few seconds)...\n"
    mapfile -t FOUND < <(scan_for_pi)
    if [[ ${#FOUND[@]} -eq 1 ]]; then
        PI="${FOUND[0]}"
    elif [[ ${#FOUND[@]} -gt 1 ]]; then
        printf "Found several Raspberry Pis:\n"
        printf "  %s\n" "${FOUND[@]}"
        printf "${YELLOW}Type the address of the Mini Pi:${NC} "
        read -r PI
    else
        print_error "No Pi found. Is it plugged into the network and powered on? Wait a minute and retry, or give its address:  ./install.sh 192.168.1.50"
    fi
fi
print_success "Pi at $PI"

# A freshly flashed Pi has a new identity: forget the old one so SSH doesn't refuse.
ssh-keygen -R "$PI" &>/dev/null || true
for _ in $(seq 1 30); do ssh_port_open "$PI" && break; sleep 2; done
ssh_port_open "$PI" || print_error "The Pi at $PI does not answer on SSH."

# ── Step 3: Set up the Pi ────────────────────────────────────────────────────
print_step "Setting up the Pi (20-40 minutes, mostly updates and building the tool)"
printf "${YELLOW}Username on the Pi [%s]:${NC} " "$USER"
read -r PI_USER
PI_USER="${PI_USER:-$USER}"

if ssh "${SSH_OPTS[@]}" -o BatchMode=yes "root@$PI" true 2>/dev/null; then
    scp "${SSH_OPTS[@]}" -q "$SCRIPT_DIR/pi/setup.sh" "root@$PI:/root/minipi-setup.sh"
    ssh "${SSH_OPTS[@]}" -t "root@$PI" bash /root/minipi-setup.sh "$PI_USER"
elif ssh "${SSH_OPTS[@]}" -o BatchMode=yes "$PI_USER@$PI" true 2>/dev/null; then
    # Already set up once (root login is closed at the end of setup).
    print_warn "The Pi is already set up."
    printf "${YELLOW}Run the whole Pi setup again? (N = skip to the next steps) [y/N]${NC} "
    read -r reply
    if [[ "$reply" =~ ^[yY] ]]; then
        scp "${SSH_OPTS[@]}" -q "$SCRIPT_DIR/pi/setup.sh" "$PI_USER@$PI:/tmp/minipi-setup.sh"
        ssh "${SSH_OPTS[@]}" -t "$PI_USER@$PI" sudo bash /tmp/minipi-setup.sh "$PI_USER"
    fi
else
    print_error "Cannot log in to $PI. Was the card made with flash.sh on this laptop?"
fi
PI_SSH=("${SSH_OPTS[@]}" -o BatchMode=yes "$PI_USER@$PI")

# ── Step 4: Backup settings on the Pi ────────────────────────────────────────
print_step "Copying your backup settings to the Pi"
if [[ $HAVE_CONFIG -eq 1 ]]; then
    # Same settings as this laptop, but the nights go to the big drive.
    sed -E "s|^([[:space:]]*backup_root[[:space:]]*=).*|\1 \"${PI_BACKUP_ROOT}\"|" "$LAPTOP_CONFIG" \
        | ssh "${PI_SSH[@]}" 'mkdir -p ~/.config/mpbackup && umask 077 && cat > ~/.config/mpbackup/config.toml'
    print_success "Settings copied (backups go to ${PI_BACKUP_ROOT})"
else
    print_warn "Skipped: no settings on this laptop"
fi

# ── Step 5: Let the Pi into the Minipass server ──────────────────────────────
print_step "Letting the Pi log in to the Minipass server"
if [[ $HAVE_CONFIG -eq 1 ]]; then
    # The backup tool refuses unknown servers: give the Pi the server identity this laptop already trusts.
    HOST="${SERVER#*@}"
    # (ssh-keygen -F fails when one of the two forms isn't known: that's fine, hence the || true.)
    KNOWN="$( { ssh-keygen -F "[$HOST]:$PORT" || true; ssh-keygen -F "$HOST" || true; } 2>/dev/null | grep -v '^#' || true)"
    if [[ -n "$KNOWN" ]]; then
        printf '%s\n' "$KNOWN" \
            | ssh "${PI_SSH[@]}" 'touch ~/.ssh/known_hosts && while read -r l; do grep -qxF "$l" ~/.ssh/known_hosts || echo "$l" >> ~/.ssh/known_hosts; done'
    fi
    PI_KEY="$(ssh "${PI_SSH[@]}" cat .ssh/id_ed25519.pub)"
    printf '%s\n' "$PI_KEY" | ssh -p "$PORT" "$SERVER" \
        'mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && read -r k && (grep -qxF "$k" ~/.ssh/authorized_keys || echo "$k" >> ~/.ssh/authorized_keys)'
    if ssh "${PI_SSH[@]}" "ssh -p $PORT -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=20 $SERVER true"; then
        print_success "The Pi can reach ${SERVER}"
    else
        print_warn "The Pi could not log in to ${SERVER}. Check the server, then run ./install.sh again."
    fi
else
    print_warn "Skipped: no server in the settings"
fi

# ── Step 6: Restart onto the new kernel ──────────────────────────────────────
# The update usually brings a new Linux kernel. Until a restart, the Pi can't load
# drivers (like the one for USB drives), so restart now, before the drive goes in.
if ! ssh "${PI_SSH[@]}" '[ -d "/usr/lib/modules/$(uname -r)" ]'; then
    print_step "Restarting the Pi onto the new kernel (your Pi password is needed)"
    ssh "${SSH_OPTS[@]}" -t "$PI_USER@$PI" sudo systemctl reboot || true
    sleep 15
    for _ in $(seq 1 60); do ssh "${PI_SSH[@]}" true 2>/dev/null && break; sleep 3; done
    ssh "${PI_SSH[@]}" true 2>/dev/null || print_error "The Pi did not come back after the restart. Give it a minute, then check it."
    print_success "Pi is back up"
fi

# ── Done ─────────────────────────────────────────────────────────────────────
printf "\n${GREEN}${BOLD}The Mini Pi is ready!${NC}\n\n"
printf "${BOLD}Next:${NC}\n"
printf "  1. Plug the big drive into the Pi\n"
printf "  2. Run:  ssh -t %s@%s sudo ~/minipi/pi/add-drive.sh\n\n" "$PI_USER" "$PI"
printf "Open the backup menu any time:  ssh -t %s@%s mpbackup\n\n" "$PI_USER" "$PI"
