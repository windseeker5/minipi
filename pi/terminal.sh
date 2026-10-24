#!/usr/bin/env bash
# terminal.sh — Ken's terminal on the Pi, from his dotfiles (no desktop)
# setup.sh runs it; to run it by hand:  ssh -t minipi.local sudo ~/minipi/pi/terminal.sh
# zsh with Powerlevel10k, Neovim (LazyVim), lazygit, fzf, nnn, bat, lsd, ripgrep, fastfetch.
# The configs are links into ~/.dotfiles, so `git pull` there (update.sh does it) updates them.
# Safe to run again.
set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
_STEP_NUM=0
print_step()   { _STEP_NUM=$((_STEP_NUM+1)); printf "\n${BLUE}${BOLD}[Step %d]${NC} %s\n" "$_STEP_NUM" "$1"; }
print_success(){ printf "${GREEN}${BOLD}[✓]${NC} %s\n" "$1"; }
print_warn()   { printf "${YELLOW}${BOLD}[!]${NC} %s\n" "$1"; }
print_error()  { printf "${RED}${BOLD}[✗]${NC} %s\n" "$1" >&2; exit 1; }

DOTFILES_URL="https://github.com/windseeker5/dotfiles.git"
# The terminal part of the dotfiles' own Pi recipe (scripts/bootstrap/minimal.sh),
# plus what LazyVim needs to build its parsers.
PACKAGES=(zsh fzf nnn bat lsd fd ripgrep lazygit fastfetch unzip wget man-db tree-sitter-cli)

# ── Step 1: Checks ───────────────────────────────────────────────────────────
print_step "Checking"
[[ $EUID -eq 0 ]] || print_error "Run with sudo:  sudo ~/minipi/pi/terminal.sh"
TARGET="${1:-${SUDO_USER:-}}"
[[ -n "$TARGET" && "$TARGET" != "root" ]] || print_error "Give the username:  sudo terminal.sh <user>"
id "$TARGET" &>/dev/null || print_error "No user '$TARGET'."
USER_HOME="$(getent passwd "$TARGET" | cut -d: -f6)"
DOTFILES="${USER_HOME}/.dotfiles"
as_user() { sudo -u "$TARGET" -H "$@"; }
print_success "Setting up the terminal for '$TARGET'"

# ── Step 2: Tools ────────────────────────────────────────────────────────────
print_step "Installing the tools"
pacman -S --needed --noconfirm "${PACKAGES[@]}"
print_success "Tools installed"

# ── Step 3: Dotfiles ─────────────────────────────────────────────────────────
print_step "Getting your dotfiles"
if [[ -d "${DOTFILES}/.git" ]]; then
    as_user git -C "$DOTFILES" pull -q --ff-only || print_warn "Could not update ${DOTFILES}, using it as is"
else
    as_user git clone -q "$DOTFILES_URL" "$DOTFILES"
fi
# Terminal configs only: nothing for Sway, Waybar or Alacritty.
as_user mkdir -p "${USER_HOME}/.config"
for pair in .zshrc:.zshrc .p10k.zsh:.p10k.zsh .config/nvim:.config/nvim; do
    src="${DOTFILES}/${pair%%:*}" dst="${USER_HOME}/${pair##*:}"
    # Keep anything that was there before and isn't already a link.
    if [[ -e "$dst" && ! -L "$dst" ]]; then
        mv "$dst" "${dst}.bak-$(date +%Y%m%d%H%M%S)"
    fi
    as_user ln -sfn "$src" "$dst"
done
print_success "Linked .zshrc, .p10k.zsh and Neovim to ${DOTFILES}"

# ── Step 4: zsh as the shell ─────────────────────────────────────────────────
print_step "Making zsh your shell"
chsh -s /usr/bin/zsh "$TARGET" >/dev/null
print_success "zsh is the shell for '$TARGET'"

# ── Step 5: Download the plugins now, not at your first login ────────────────
print_step "Downloading zsh and Neovim plugins (a few minutes)"
as_user zsh -i -c 'exit' >/dev/null 2>&1 || print_warn "zsh plugins: they will finish at your first login"
as_user timeout 900 nvim --headless '+Lazy! restore' +qa >/dev/null 2>&1 \
    || print_warn "Neovim plugins: they will finish the first time you open nvim"
print_success "Plugins ready"

printf "\n${GREEN}${BOLD}Terminal ready!${NC} Log in again to get zsh.\n\n"
