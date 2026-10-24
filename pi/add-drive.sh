#!/usr/bin/env bash
# add-drive.sh — Use the big USB drive for backups and start the nightly backup
# Run on the Pi with the drive plugged in:  sudo ~/minipi/pi/add-drive.sh
set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
_STEP_NUM=0
print_step()   { _STEP_NUM=$((_STEP_NUM+1)); printf "\n${BLUE}${BOLD}[Step %d]${NC} %s\n" "$_STEP_NUM" "$1"; }
print_success(){ printf "${GREEN}${BOLD}[✓]${NC} %s\n" "$1"; }
print_warn()   { printf "${YELLOW}${BOLD}[!]${NC} %s\n" "$1"; }
print_error()  { printf "${RED}${BOLD}[✗]${NC} %s\n" "$1" >&2; exit 1; }

MNT="/mnt/backup"
BACKUP_DIR="${MNT}/minipass"
LABEL="minipi-backup"

# ── Step 1: Checks ───────────────────────────────────────────────────────────
print_step "Checking"
[[ $EUID -eq 0 ]] || print_error "Run with sudo:  sudo ~/minipi/pi/add-drive.sh"
OWNER="${SUDO_USER:-}"
[[ -n "$OWNER" && "$OWNER" != "root" ]] || print_error "Run it with sudo from your own user, not as root."
OWNER_HOME="$(getent passwd "$OWNER" | cut -d: -f6)"
CONFIG="${OWNER_HOME}/.config/mpbackup/config.toml"
print_success "Backups will belong to '$OWNER'"

# ── Step 2: The drive ────────────────────────────────────────────────────────
if mountpoint -q "$MNT"; then
    print_step "Drive already in place"
    df -h "$MNT" | sed 's/^/  /'
else
    print_step "Pick the drive"
    # Every disk except the SD card the Pi runs from.
    mapfile -t DISKS < <(lsblk -dpno NAME,TYPE | awk '$2 == "disk" && $1 !~ /mmcblk|zram/ { print $1 }')
    [[ ${#DISKS[@]} -gt 0 ]] || print_error "No drive found. Plug it in (with its own power if it has a plug), wait 10 seconds, retry."
    lsblk -dpo NAME,SIZE,MODEL,TRAN "${DISKS[@]}" | sed 's/^/  /'
    if [[ ${#DISKS[@]} -eq 1 ]]; then
        DISK="${DISKS[0]}"
    else
        printf "${YELLOW}Type the drive (e.g. /dev/sda):${NC} "
        read -r DISK
        DISK="/dev/${DISK#/dev/}"
    fi
    [[ " ${DISKS[*]} " == *" $DISK "* ]] || print_error "'$DISK' is not in the list."
    printf "\nWhat is on it now:\n"
    lsblk -po NAME,SIZE,FSTYPE,LABEL "$DISK" | sed 's/^/  /'

    # A Linux filesystem already on it can be kept. Others (NTFS, exFAT) can't do the
    # hard links the backup uses to store unchanged files only once, so they get erased.
    PART=""
    KEEP="$(lsblk -pnlo NAME,FSTYPE "$DISK" | awk '$2 ~ /^(ext4|btrfs|xfs)$/ { print $1; exit }')"
    if [[ -n "$KEEP" ]]; then
        printf "\n${YELLOW}Keep the files on %s and use it as is? [y/N]${NC} " "$KEEP"
        read -r reply
        [[ "$reply" =~ ^[yY] ]] && PART="$KEEP"
    fi

    if [[ -z "$PART" ]]; then
        printf "\n${RED}${BOLD}EVERYTHING ON %s WILL BE ERASED.${NC}\n" "$DISK"
        printf "${YELLOW}Type 'yes' to continue:${NC} "
        read -r CONFIRM
        [[ "$CONFIRM" == "yes" ]] || { printf "Stopped, nothing changed.\n"; exit 0; }
        print_step "Erasing and formatting $DISK (a minute or two)"
        umount "${DISK}"?* 2>/dev/null || true
        wipefs -aq "$DISK"
        parted -s "$DISK" mklabel gpt mkpart "$LABEL" ext4 1MiB 100%
        partprobe "$DISK" 2>/dev/null || true
        udevadm settle
        PART="$(lsblk -pnlo NAME,TYPE "$DISK" | awk '$2 == "part" { print $1; exit }')"
        [[ -n "$PART" ]] || print_error "The new partition did not show up."
        mkfs.ext4 -qF -m 0 -L "$LABEL" "$PART"
        print_success "Formatted $PART"
    fi

    print_step "Mounting the drive at $MNT"
    FSTYPE="$(blkid -s TYPE -o value "$PART")"
    UUID="$(blkid -s UUID -o value "$PART")"
    mkdir -p "$MNT"
    # With the drive missing, nothing may write here: a backup then fails loudly
    # (and sends an alert) instead of quietly filling up the SD card.
    chattr +i "$MNT"
    sed -i "\| ${MNT} |d" /etc/fstab
    # nofail: the Pi still starts if the drive is unplugged.
    echo "UUID=${UUID} ${MNT} ${FSTYPE} defaults,noatime,nofail,x-systemd.device-timeout=10s 0 2" >> /etc/fstab
    systemctl daemon-reload
    mount "$MNT"
    print_success "Mounted, and it will come back by itself after a restart"
    df -h "$MNT" | sed 's/^/  /'
fi

# ── Step 3: Backup folder and settings ───────────────────────────────────────
print_step "Pointing the backup at the drive"
install -d -o "$OWNER" -g "$OWNER" "$BACKUP_DIR"
if [[ -f "$CONFIG" ]]; then
    sed -i -E "s|^([[:space:]]*backup_root[[:space:]]*=).*|\1 \"${BACKUP_DIR}\"|" "$CONFIG"
    print_success "Backups go to $BACKUP_DIR"
else
    print_warn "No settings yet at $CONFIG. Run ./install.sh from the laptop first."
fi

# ── Step 4: Nightly timer ────────────────────────────────────────────────────
print_step "Starting the nightly backup"
systemctl --user -M "${OWNER}@" enable --now mpbackup.timer
NEXT="$(systemctl --user -M "${OWNER}@" show mpbackup.timer -p NextElapseUSecRealtime --value)"
print_success "Next backup: ${NEXT:-tonight at 3:00}"

printf "\n${GREEN}${BOLD}The Mini Pi is backing up!${NC}\n\n"
printf "  Run one now to try it:  mpbackup run\n"
printf "  Open the menu:          mpbackup\n\n"
