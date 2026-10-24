#!/usr/bin/env bash
# screen.sh — Turn on the 3.2" screen that sits on the Pi's pins
# Run on the Pi:  sudo ~/minipi/pi/screen.sh   then restart:  sudo reboot
#
# Arch ARM boots through U-Boot with the plain Linux device tree, which knows nothing
# about the screen. This adds the screen (pi/screen/minipi-lcd.dts) to that device tree,
# keeps the original next to it, and re-adds it after every kernel update.
set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
_STEP_NUM=0
print_step()   { _STEP_NUM=$((_STEP_NUM+1)); printf "\n${BLUE}${BOLD}[Step %d]${NC} %s\n" "$_STEP_NUM" "$1"; }
print_success(){ printf "${GREEN}${BOLD}[✓]${NC} %s\n" "$1"; }
print_error()  { printf "${RED}${BOLD}[✗]${NC} %s\n" "$1" >&2; exit 1; }

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DTBO="/boot/minipi-lcd.dtbo"
HELPER="/usr/local/bin/minipi-lcd-dtb"
HOOK="/etc/pacman.d/hooks/minipi-lcd.hook"

# ── Step 1: Checks ───────────────────────────────────────────────────────────
print_step "Checking"
[[ $EUID -eq 0 ]] || print_error "Run with sudo:  sudo ~/minipi/pi/screen.sh"
OWNER="${SUDO_USER:-}"
print_success "Ready"

# ── Step 2: Device tree tools ────────────────────────────────────────────────
print_step "Installing the device tree tools"
pacman -S --needed --noconfirm dtc >/dev/null
print_success "dtc installed"

# ── Step 3: The screen description ───────────────────────────────────────────
print_step "Building the screen description"
dtc -@ -q -I dts -O dtb -o "$DTBO" "$REPO/pi/screen/minipi-lcd.dts"
print_success "$DTBO"

# ── Step 4: Add it to the boot files, now and after every kernel update ──────
print_step "Adding the screen to the boot files"
cat > "$HELPER" <<'EOF'
#!/usr/bin/env bash
# Adds the Mini Pi screen to the Pi 4 device tree (run by screen.sh and the pacman hook).
set -euo pipefail
DTB=/boot/dtbs/broadcom/bcm2711-rpi-4-b.dtb
# No screen in the current file: it's the stock one (first run or fresh from a kernel update).
if ! fdtget "$DTB" /soc/spi@7e204000/display@0 compatible &>/dev/null; then
    cp "$DTB" "$DTB.orig"
fi
fdtoverlay -i "$DTB.orig" -o "$DTB.new" /boot/minipi-lcd.dtbo
mv "$DTB.new" "$DTB"
echo "Mini Pi screen added to $DTB (original kept as $DTB.orig)"
EOF
chmod 755 "$HELPER"

mkdir -p "$(dirname "$HOOK")"
cat > "$HOOK" <<EOF
[Trigger]
Operation = Install
Operation = Upgrade
Type = Path
Target = boot/dtbs/broadcom/bcm2711-rpi-4-b.dtb

[Action]
Description = Adding the Mini Pi screen to the device tree...
When = PostTransaction
Exec = $HELPER
EOF
"$HELPER"
print_success "Boot files updated (kept after kernel updates)"

# ── Step 5: Show the backup status on it ─────────────────────────────────────
if [[ -n "$OWNER" && "$OWNER" != "root" ]]; then
    print_step "Showing the backup status on the screen"
    usermod -aG video "$OWNER"
    # Starts with the Pi; until the restart the screen isn't there yet, so it just retries.
    systemctl --user -M "${OWNER}@" daemon-reload
    systemctl --user -M "${OWNER}@" enable mptool-screen.service
    print_success "Logo and backup status will show after the restart"
fi

printf "\n${GREEN}${BOLD}Screen set up.${NC} Restart to turn it on:  sudo reboot\n"
printf "If the Pi does not start again: put the SD card in the laptop and, on its BOOT partition,\n"
printf "copy dtbs/broadcom/bcm2711-rpi-4-b.dtb.orig over dtbs/broadcom/bcm2711-rpi-4-b.dtb\n\n"
