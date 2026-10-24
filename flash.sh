#!/usr/bin/env bash
# flash.sh — Put Arch Linux ARM on an SD card for the Mini Pi (Raspberry Pi 3/4, 64-bit)
# Run on your LAPTOP with the SD card plugged in:  sudo ./flash.sh
#
# Besides the plain Arch install, the card comes out ready for install.sh:
#   - hostname "minipi" and your laptop's timezone
#   - your laptop's SSH key, so install.sh can log in with no password
#   - your Wi-Fi, if you give it (or plug in a network cable instead)
set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
_STEP_NUM=0
print_step()   { _STEP_NUM=$((_STEP_NUM+1)); printf "\n${BLUE}${BOLD}[Step %d]${NC} %s\n" "$_STEP_NUM" "$1"; }
print_success(){ printf "${GREEN}${BOLD}[✓]${NC} %s\n" "$1"; }
print_error()  { printf "${RED}${BOLD}[✗]${NC} %s\n" "$1" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOSTNAME_PI="minipi"
TARBALL_NAME="ArchLinuxARM-rpi-aarch64-latest.tar.gz"
TARBALL_URL="http://os.archlinuxarm.org/os/${TARBALL_NAME}"
CACHE_DIR="${SCRIPT_DIR}/cache"
WORK="$(mktemp -d /tmp/minipi-flash.XXXXXX)"
BOOT_MNT="${WORK}/boot"
ROOT_MNT="${WORK}/root"

# If anything fails half way, don't leave the card mounted.
cleanup() {
    umount "$BOOT_MNT" 2>/dev/null || true
    umount "$ROOT_MNT" 2>/dev/null || true
    rmdir "$BOOT_MNT" "$ROOT_MNT" "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

# ── Step 1: Checks ───────────────────────────────────────────────────────────
print_step "Checking privileges and tools"
[[ $EUID -eq 0 ]] || print_error "Run with sudo:  sudo ./flash.sh"

MISSING=()
for cmd in parted mkfs.fat mkfs.ext4 bsdtar wget md5sum blkid wpa_passphrase; do
    command -v "$cmd" &>/dev/null || MISSING+=("$cmd")
done
if [[ ${#MISSING[@]} -gt 0 ]]; then
    printf "${RED}Missing commands:${NC} %s\n" "${MISSING[*]}"
    printf "${YELLOW}Install on Arch:${NC} sudo pacman -S parted dosfstools e2fsprogs libarchive wget wpa_supplicant\n"
    print_error "Install missing tools and re-run."
fi

# The person who typed sudo: their home holds the SSH key we put on the card.
REAL_USER="${SUDO_USER:-$USER}"
REAL_HOME="$(getent passwd "$REAL_USER" | cut -d: -f6)"
PUBKEY=""
for k in "$REAL_HOME/.ssh/id_ed25519.pub" "$REAL_HOME/.ssh/id_rsa.pub"; do
    [[ -f "$k" ]] && { PUBKEY="$k"; break; }
done
[[ -n "$PUBKEY" ]] || print_error "No SSH key in $REAL_HOME/.ssh. Create one with:  ssh-keygen -t ed25519"
print_success "Ready (SSH key: $PUBKEY)"

# ── Step 2: Get the Arch Linux ARM image ─────────────────────────────────────
print_step "Getting the Arch Linux ARM image"
mkdir -p "$CACHE_DIR"
TARBALL="${CACHE_DIR}/${TARBALL_NAME}"
if [[ ! -f "$TARBALL" && -f "$REAL_HOME/Downloads/${TARBALL_NAME}" ]]; then
    TARBALL="$REAL_HOME/Downloads/${TARBALL_NAME}"
fi
if [[ ! -f "$TARBALL" ]]; then
    printf "Downloading (~700 MB, kept in cache/ for next time)...\n"
    wget -c --show-progress "$TARBALL_URL" -O "${TARBALL}.part"
    mv "${TARBALL}.part" "$TARBALL"
fi

# Check the download is complete and not damaged.
if wget -q "${TARBALL_URL}.md5" -O "${WORK}/image.md5"; then
    WANT="$(cut -d' ' -f1 "${WORK}/image.md5")"
    HAVE="$(md5sum "$TARBALL" | cut -d' ' -f1)"
    if [[ "$WANT" != "$HAVE" ]]; then
        printf "${YELLOW}The image does not match the official checksum.${NC}\n"
        printf "It is damaged or a newer one came out. Delete it and run again:\n  rm %s\n" "$TARBALL"
        exit 1
    fi
    print_success "Image checked: $TARBALL"
else
    printf "${YELLOW}Could not fetch the checksum (offline?), using the image as is.${NC}\n"
fi
chown -R "$REAL_USER:" "$CACHE_DIR" 2>/dev/null || true

# ── Step 3: Pick the SD card ─────────────────────────────────────────────────
print_step "Pick the SD card"
lsblk -d -o NAME,SIZE,MODEL,TRAN,RM -e 7,11 | sed 's/^/  /'
printf "\n${YELLOW}Type the SD card (e.g. sdb or mmcblk0):${NC} "
read -r DEV
DEVICE="/dev/${DEV#/dev/}"
[[ -b "$DEVICE" && "$(lsblk -dno TYPE "$DEVICE")" == "disk" ]] || print_error "'$DEVICE' is not a disk."

# Never touch the disk this laptop runs from.
if lsblk -no MOUNTPOINTS "$DEVICE" | grep -qxE '/|/boot|/boot/efi|/home|/efi|\[SWAP\]'; then
    print_error "$DEVICE is this laptop's own disk. Pick the SD card."
fi

SIZE="$(lsblk -dno SIZE "$DEVICE" | tr -d ' ')"
MODEL="$(lsblk -dno MODEL "$DEVICE" | sed 's/ *$//')"
printf "\n${RED}${BOLD}EVERYTHING ON %s (%s %s) WILL BE ERASED.${NC}\n" "$DEVICE" "$SIZE" "$MODEL"
printf "${YELLOW}Type 'yes' to continue:${NC} "
read -r CONFIRM
[[ "$CONFIRM" == "yes" ]] || { printf "Stopped, nothing changed.\n"; exit 0; }

# Asked now, so the long part after runs without you.
printf "\n${YELLOW}Wi-Fi name for the Pi (empty = network cable only):${NC} "
read -r SSID
PSK_BLOCK=""
if [[ -n "$SSID" ]]; then
    printf "${YELLOW}Password (not shown):${NC} "
    read -rs PASS
    printf "\n"
    printf "${YELLOW}Country code [CA]:${NC} "
    read -r COUNTRY
    COUNTRY="${COUNTRY:-CA}"
    # wpa_passphrase stores the password scrambled (and refuses one under 8 letters).
    PSK_BLOCK="$(wpa_passphrase "$SSID" "$PASS" | grep -v '^[[:space:]]*#psk=')" \
        || print_error "Wi-Fi passwords are 8 to 63 characters. Nothing changed."
fi

# ── Step 4: Partition and format ─────────────────────────────────────────────
print_step "Partitioning and formatting $DEVICE"
# Unmount every partition on the card (the desktop may have mounted some by itself).
unmount_card() {
    local p
    for p in $(lsblk -lnpo NAME "$DEVICE" | tail -n +2); do
        while findmnt -S "$p" &>/dev/null; do umount "$p"; done
    done
}
unmount_card
# Erase the old partitions' signatures first: the new partitions land in the same
# spots, and the desktop would otherwise find the old files there and mount them.
for p in $(lsblk -lnpo NAME "$DEVICE" | tail -n +2); do wipefs -aq "$p"; done
wipefs -aq "$DEVICE"
parted -s "$DEVICE" mklabel msdos
parted -s "$DEVICE" mkpart primary fat32 1MiB 513MiB
parted -s "$DEVICE" mkpart primary ext4 513MiB 100%
parted -s "$DEVICE" set 1 boot on
partprobe "$DEVICE" 2>/dev/null || true
udevadm settle 2>/dev/null || sleep 2
unmount_card

if [[ "$DEVICE" == *[0-9] ]]; then   # mmcblk0, nvme0n1 → partitions get a "p"
    PART1="${DEVICE}p1"; PART2="${DEVICE}p2"
else
    PART1="${DEVICE}1"; PART2="${DEVICE}2"
fi
mkfs.fat -F32 -n BOOT "$PART1" >/dev/null
mkfs.ext4 -qF -L minipi-root "$PART2"
udevadm settle 2>/dev/null || sleep 2
unmount_card
print_success "Formatted"

# ── Step 5: Copy Arch onto the card ──────────────────────────────────────────
print_step "Copying Arch Linux onto the card (a few minutes)"
mkdir -p "$BOOT_MNT" "$ROOT_MNT"
mount "$PART2" "$ROOT_MNT"
mount "$PART1" "$BOOT_MNT"
bsdtar -xpf "$TARBALL" -C "$ROOT_MNT"
mv "${ROOT_MNT}/boot/"* "$BOOT_MNT/"
print_success "Copied"

# ── Step 6: Make it boot on any Pi ───────────────────────────────────────────
# The SD card is called mmcblk0 on some kernels and mmcblk1 on others.
# Pointing at the partitions by their ID works with both.
print_step "Pointing the boot files at the card's partitions"
BOOT_UUID="$(blkid -s UUID -o value "$PART1")"
ROOT_PARTUUID="$(blkid -s PARTUUID -o value "$PART2")"
sed -i -E "s|^/dev/mmcblk[01]p1([[:space:]])|UUID=${BOOT_UUID}\1|" "${ROOT_MNT}/etc/fstab"
if [[ -f "${BOOT_MNT}/cmdline.txt" ]]; then
    sed -i -E "s|root=/dev/mmcblk[01]p2|root=PARTUUID=${ROOT_PARTUUID}|" "${BOOT_MNT}/cmdline.txt"
fi
grep -q "UUID=${BOOT_UUID}" "${ROOT_MNT}/etc/fstab" || print_error "Could not update /etc/fstab on the card."
print_success "Boot partition: UUID=${BOOT_UUID}"

# ── Step 7: Get the card ready for install.sh ────────────────────────────────
print_step "Name, timezone and SSH key"
echo "$HOSTNAME_PI" > "${ROOT_MNT}/etc/hostname"

TZ_NAME="$(timedatectl show -p Timezone --value 2>/dev/null || true)"
if [[ -n "$TZ_NAME" && -f "${ROOT_MNT}/usr/share/zoneinfo/${TZ_NAME}" ]]; then
    ln -sf "/usr/share/zoneinfo/${TZ_NAME}" "${ROOT_MNT}/etc/localtime"
fi

# Root may log in over SSH with a key only (never a password): this lets install.sh in.
install -d -m 700 "${ROOT_MNT}/root/.ssh"
install -m 600 "$PUBKEY" "${ROOT_MNT}/root/.ssh/authorized_keys"
chown -R 0:0 "${ROOT_MNT}/root/.ssh"
print_success "Hostname ${HOSTNAME_PI}, timezone ${TZ_NAME:-UTC}, key from ${REAL_USER}"

# Same files as pi/wifi.sh. The higher metric means: a cable wins while it's plugged in.
if [[ -n "$PSK_BLOCK" ]]; then
    (umask 077; printf 'ctrl_interface=/run/wpa_supplicant\nupdate_config=1\ncountry=%s\n%s\n' "$COUNTRY" "$PSK_BLOCK" \
        > "${ROOT_MNT}/etc/wpa_supplicant/wpa_supplicant-wlan0.conf")
    cat > "${ROOT_MNT}/etc/systemd/network/25-wlan.network" <<EOF
[Match]
Name=wlan0

[Network]
DHCP=yes
DNSSEC=no

[DHCPv4]
RouteMetric=2048

[IPv6AcceptRA]
RouteMetric=2048
EOF
    # What "systemctl enable wpa_supplicant@wlan0" would do on the Pi.
    ln -sf /usr/lib/systemd/system/wpa_supplicant@.service \
        "${ROOT_MNT}/etc/systemd/system/multi-user.target.wants/wpa_supplicant@wlan0.service"
    print_success "Wi-Fi \"${SSID}\" set"
fi

# ── Step 8: Finish ───────────────────────────────────────────────────────────
print_step "Writing everything to the card (please wait)"
sync
umount "$BOOT_MNT"
umount "$ROOT_MNT"
unmount_card
print_success "Done, you can remove the card"

printf "\n${GREEN}${BOLD}SD card ready!${NC}\n\n"
printf "${BOLD}Next:${NC}\n"
if [[ -n "$PSK_BLOCK" ]]; then
    printf "  1. Put the card in the Pi, then plug in the power (no cable needed)\n"
else
    printf "  1. Put the card in the Pi, plug in the network cable, then the power\n"
fi
printf "  2. Wait about a minute, then on this laptop run:  ./install.sh\n\n"
