#!/usr/bin/env bash
# wifi.sh — Put the Mini Pi on Wi-Fi (the network cable keeps working as a backup)
# Run on the Pi, cable still plugged in:  ssh -t minipi sudo ~/minipi/pi/wifi.sh
# Run it again to change the network or the password.
set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
_STEP_NUM=0
print_step()   { _STEP_NUM=$((_STEP_NUM+1)); printf "\n${BLUE}${BOLD}[Step %d]${NC} %s\n" "$_STEP_NUM" "$1"; }
print_success(){ printf "${GREEN}${BOLD}[✓]${NC} %s\n" "$1"; }
print_error()  { printf "${RED}${BOLD}[✗]${NC} %s\n" "$1" >&2; exit 1; }

WLAN="wlan0"
CONF="/etc/wpa_supplicant/wpa_supplicant-${WLAN}.conf"
# Same file flash.sh writes. The higher metric means: the cable wins while it's plugged in.
NETWORK="/etc/systemd/network/25-wlan.network"

# ── Step 1: Checks ───────────────────────────────────────────────────────────
print_step "Checking"
[[ $EUID -eq 0 ]] || print_error "Run with sudo:  sudo ~/minipi/pi/wifi.sh"
ip link show "$WLAN" &>/dev/null || print_error "This Pi has no Wi-Fi ($WLAN not found)."
print_success "Wi-Fi chip found"

# ── Step 2: Which network ────────────────────────────────────────────────────
print_step "Choosing the network"
ip link set "$WLAN" up
printf "Networks nearby:\n"
iw dev "$WLAN" scan 2>/dev/null | sed -n 's/^[[:space:]]*SSID: //p' | grep -v '^$' | sort -u | head -15 | sed 's/^/  /' || true
printf "\n${YELLOW}Wi-Fi name:${NC} "
read -r SSID
[[ -n "$SSID" ]] || print_error "No name given, nothing changed."
printf "${YELLOW}Password (not shown):${NC} "
read -rs PASS
printf "\n"
OLD_COUNTRY="$(sed -n 's/^country=//p' "$CONF" 2>/dev/null || true)"
printf "${YELLOW}Country code [%s]:${NC} " "${OLD_COUNTRY:-CA}"
read -r COUNTRY
COUNTRY="${COUNTRY:-${OLD_COUNTRY:-CA}}"

# ── Step 3: Save it and turn Wi-Fi on ────────────────────────────────────────
print_step "Saving and turning Wi-Fi on"
# wpa_passphrase stores the password scrambled (and refuses one under 8 letters).
PSK_BLOCK="$(wpa_passphrase "$SSID" "$PASS" | grep -v '^[[:space:]]*#psk=')" \
    || print_error "Wi-Fi passwords are 8 to 63 characters. Nothing changed."
(umask 077; printf 'ctrl_interface=/run/wpa_supplicant\nupdate_config=1\ncountry=%s\n%s\n' "$COUNTRY" "$PSK_BLOCK" > "$CONF")
cat > "$NETWORK" <<EOF
[Match]
Name=${WLAN}

[Network]
DHCP=yes
DNSSEC=no

[DHCPv4]
RouteMetric=2048

[IPv6AcceptRA]
RouteMetric=2048
EOF
systemctl enable -q "wpa_supplicant@${WLAN}.service"
systemctl restart "wpa_supplicant@${WLAN}.service"
networkctl reload
networkctl reconfigure "$WLAN"
print_success "Saved for \"$SSID\""

# ── Step 4: Wait for an address ──────────────────────────────────────────────
print_step "Connecting (up to 40 seconds)"
IP=""
for _ in $(seq 1 40); do
    IP="$(ip -4 -o addr show "$WLAN" | awk '{print $4}' | cut -d/ -f1 | head -1)"
    [[ -n "$IP" ]] && break
    sleep 1
done
if [[ -z "$IP" ]]; then
    journalctl -u "wpa_supplicant@${WLAN}" -n 5 --no-pager | sed 's/^/  /'
    print_error "No connection. Check the name and password, then run again. The cable still works."
fi
print_success "On Wi-Fi at $IP"

printf "\n${GREEN}${BOLD}Wi-Fi is on!${NC}\n\n"
printf "Unplug the cable whenever you like. Then log in with:  ssh minipi   (or:  ssh %s@%s)\n" "${SUDO_USER:-$USER}" "$IP"
printf "The little screen shows the Wi-Fi address within a minute of unplugging.\n\n"
