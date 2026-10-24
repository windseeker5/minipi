#!/usr/bin/env bash
# setup.sh — Turn a freshly flashed Arch Linux ARM Pi into the Mini Pi backup box
# install.sh copies this to the Pi and runs it as root:  bash setup.sh <username>
# Safe to run again: every step skips what is already done.
set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
_STEP_NUM=0
print_step()   { _STEP_NUM=$((_STEP_NUM+1)); printf "\n${BLUE}${BOLD}[Step %d]${NC} %s\n" "$_STEP_NUM" "$1"; }
print_success(){ printf "${GREEN}${BOLD}[✓]${NC} %s\n" "$1"; }
print_warn()   { printf "${YELLOW}${BOLD}[!]${NC} %s\n" "$1"; }
print_error()  { printf "${RED}${BOLD}[✗]${NC} %s\n" "$1" >&2; exit 1; }

REPO_URL="https://github.com/windseeker5/minipi.git"
MIRROR='https://mirrors.ocf.berkeley.edu/archlinuxarm/$arch/$repo'
PACKAGES=(sudo git base-devel rust rsync sqlite openssh curl neovim btop)

# ── Step 1: Checks ───────────────────────────────────────────────────────────
print_step "Checking"
[[ $EUID -eq 0 ]] || print_error "Run as root."
NEW_USER="${1:-}"
if [[ -z "$NEW_USER" ]]; then
    printf "${YELLOW}Username to create:${NC} "
    read -r NEW_USER
fi
[[ "$NEW_USER" =~ ^[a-z_][a-z0-9_-]*$ ]] || print_error "'$NEW_USER' is not a valid username."
USER_HOME="/home/${NEW_USER}"
cd /
print_success "Setting up for user '$NEW_USER'"

# ── Step 2: Clock and DNS ────────────────────────────────────────────────────
# The Pi has no clock battery: downloads fail until the time is right.
print_step "Setting the clock and name lookups"
timedatectl set-ntp true
for _ in $(seq 1 30); do
    [[ "$(timedatectl show -p NTPSynchronized --value)" == "yes" ]] && break
    sleep 2
done
# Arch ARM ships a broken resolv.conf: hand it to systemd-resolved.
if [[ ! -L /etc/resolv.conf ]]; then
    systemctl enable --now systemd-resolved
    ln -sf /run/systemd/resolve/resolv.conf /etc/resolv.conf
fi
print_success "Time: $(date '+%Y-%m-%d %H:%M %Z')"

# ── Step 3: Language ─────────────────────────────────────────────────────────
print_step "Setting UTF-8"
sed -i 's/^#en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen
locale-gen >/dev/null
echo 'LANG=en_US.UTF-8' > /etc/locale.conf
export LANG=en_US.UTF-8
print_success "Language en_US.UTF-8"

# ── Step 4: Update Arch and install packages ─────────────────────────────────
print_step "Updating Arch and installing packages (the long part)"
if [[ ! -f /etc/pacman.d/gnupg/trustdb.gpg ]]; then
    pacman-key --init
    pacman-key --populate archlinuxarm
fi
echo "Server = ${MIRROR}" > /etc/pacman.d/mirrorlist
pacman -Syu --noconfirm
pacman -S --needed --noconfirm "${PACKAGES[@]}"
print_success "System up to date"

# ── Step 5: Your user ────────────────────────────────────────────────────────
print_step "Creating user '$NEW_USER'"
if id "$NEW_USER" &>/dev/null; then
    print_success "User already exists"
else
    useradd -m -G wheel -s /bin/bash "$NEW_USER"
    printf "${YELLOW}Choose a password for %s (used for sudo):${NC}\n" "$NEW_USER"
    until passwd "$NEW_USER"; do :; done
    print_success "User created"
fi
echo '%wheel ALL=(ALL:ALL) ALL' > /etc/sudoers.d/10-wheel
chmod 440 /etc/sudoers.d/10-wheel
visudo -cqf /etc/sudoers.d/10-wheel || print_error "sudo rule is invalid"

# Same SSH key as root, so the laptop logs in as the user with no password.
install -d -m 700 -o "$NEW_USER" -g "$NEW_USER" "${USER_HOME}/.ssh"
if [[ -f /root/.ssh/authorized_keys ]]; then
    touch "${USER_HOME}/.ssh/authorized_keys"
    while read -r key; do
        grep -qxF "$key" "${USER_HOME}/.ssh/authorized_keys" || echo "$key" >> "${USER_HOME}/.ssh/authorized_keys"
    done < /root/.ssh/authorized_keys
    chown "$NEW_USER:" "${USER_HOME}/.ssh/authorized_keys"
    chmod 600 "${USER_HOME}/.ssh/authorized_keys"
fi
# The Pi's own key, used to log in to the Minipass server.
if [[ ! -f "${USER_HOME}/.ssh/id_ed25519" ]]; then
    sudo -u "$NEW_USER" ssh-keygen -q -t ed25519 -N "" -C "${NEW_USER}@$(cat /etc/hostname)" -f "${USER_HOME}/.ssh/id_ed25519"
fi
# Keep the user's timers running with nobody logged in.
loginctl enable-linger "$NEW_USER"
print_success "User ready (sudo, SSH key, timers on)"

# ── Step 6: The backup tool ──────────────────────────────────────────────────
print_step "Getting and building the backup tool (10-20 minutes on a Pi)"
if [[ ! -d "${USER_HOME}/minipi/.git" ]]; then
    sudo -u "$NEW_USER" git clone -q "$REPO_URL" "${USER_HOME}/minipi"
fi
sudo -u "$NEW_USER" -H bash "${USER_HOME}/minipi/pi/update.sh"
ln -sf "${USER_HOME}/.local/bin/mpbackup" /usr/local/bin/mpbackup
CONFIG="${USER_HOME}/.config/mpbackup/config.toml"
if [[ ! -f "$CONFIG" ]]; then
    sudo -u "$NEW_USER" install -D -m 600 "${USER_HOME}/minipi/backup/config.example.toml" "$CONFIG"
fi
print_success "mpbackup installed (the nightly timer starts once the big drive is added)"

# ── Step 7: Close the default doors ──────────────────────────────────────────
# Arch ARM ships with alarm/alarm and root/root, known to everyone.
print_step "Locking the default accounts"
passwd -l alarm &>/dev/null || true
passwd -l root &>/dev/null
rm -f /root/.ssh/authorized_keys
print_success "Default passwords locked, root login closed. Use '$NEW_USER' with sudo from now on."

printf "\n${GREEN}${BOLD}Pi setup complete!${NC}\n"
