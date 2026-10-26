# Mini Pi

A Raspberry Pi with a big drive that looks after the Minipass server: it backs it up
every night and checks every customer's site every 15 minutes. It runs Arch Linux ARM
(no desktop) and **mptool**, the Minipass tool in `mptool/`.

Works on a Raspberry Pi 3 or 4 (64-bit). Not on a Pi 5.

## Recipe

All commands run on the laptop, from this folder.

**1. Make the SD card** (plug the card into the laptop):

```bash
sudo ./flash.sh
```

It asks for your Wi-Fi name and password. Leave the name empty to use a network cable instead.

**2. Start the Pi.** Put the card in (and the network cable, if no Wi-Fi), then the power. Wait a minute.

**3. Set it up** (finds the Pi by itself, takes 20 to 40 minutes):

```bash
./install.sh
```

It asks for a username and a password for the Pi, then does the rest:
updates Arch, builds mptool, sets up your terminal from your
[dotfiles](https://github.com/windseeker5/dotfiles) (zsh, Neovim, lazygit, fzf, nnn; no desktop),
copies your settings (`~/.config/mptool/config.toml`) and lets the Pi log in to the Minipass server.

**4. Add the big drive** (plug it into the Pi, with its own power if it has a plug):

```bash
ssh -t minipi sudo ~/minipi/pi/add-drive.sh
```

Done: a backup runs every night at 3:00, and Discord tells you how it went.

## Wi-Fi

To put a Pi that runs on the cable onto Wi-Fi, or to change the Wi-Fi network or password:

```bash
ssh -t minipi 'cd ~/minipi && git pull -q && sudo pi/wifi.sh'
```

Keep the cable plugged in while it runs. When it says Wi-Fi is on, unplug the cable.
If both are plugged in, the cable is used; the little screen always shows the address in use.

## Site checks

Every 15 minutes the Pi loads each customer's login page (`https://<customer>.minipass.me/login`)
and checks that it answers, that the login form is really there, and that the certificate
has more than 14 days left. A site that fails is checked again a minute later; if it still
fails, Discord gets an alert right away, and another when it's back. Customers come from the last backup, so new ones are picked up overnight.
Settings (all optional) are at the end of `mptool/config.example.toml`.

## Every day

| What | Command |
|------|---------|
| Open the backup menu | `ssh -t minipi mptool` |
| Run a backup now | `ssh minipi mptool run` |
| Check the sites now | `ssh minipi mptool monitor` |
| Get the newest mptool from GitHub | `ssh minipi '~/minipi/pi/update.sh'` |

If `ssh minipi` doesn't work (it can lag a day after switching to Wi-Fi), use `ssh minipi.local`,
or the address on the little screen, e.g. `ssh 192.168.1.50`.

## What's in here

| File | Runs on | Does |
|------|---------|------|
| `flash.sh` | laptop | Puts Arch on the SD card, with your SSH key on it |
| `install.sh` | laptop | Finds the Pi and sets it all up over the network |
| `pi/setup.sh` | Pi | Updates Arch, creates your user, installs the tool |
| `pi/update.sh` | Pi | Pulls the latest tool from GitHub and rebuilds it |
| `pi/add-drive.sh` | Pi | Prepares the big drive and starts the nightly backup |
| `pi/screen.sh` | Pi | Turns on the 3.2" screen on the pins (then restart) |
| `pi/wifi.sh` | Pi | Puts the Pi on Wi-Fi, or changes the network |
| `pi/terminal.sh` | Pi | Your terminal from the dotfiles: zsh, Neovim, lazygit, fzf, nnn... |
| `mptool/` | | The Minipass tool (Rust): backup, monitoring, screen |

Secrets (the Discord link, server keys) never go in this repo. They live only in
`~/.config/mptool/config.toml` on the laptop and on the Pi.

## The little screen

The 3.2" 320x240 screen on the pins (a Waveshare 3.2" (B) copy) needs one extra step:

```bash
ssh -t minipi 'sudo ~/minipi/pi/screen.sh && sudo reboot'
```

It then loops: the Minipass logo opens out from the middle and "minipass management tool"
types itself (5 seconds), then the customers go by 5 per page, 10 seconds a page.
The top line has the Pi's address (to SSH in) and the clock. Each row: a green, yellow or red
icon, the subdomain, last night's files and backup size, and the login page time.
Broken customers come first, with the broken value in red. The bottom bar shows how much
of the backup drive the backups use. Last comes a flip clock (20 seconds): its cards flip
from blank to the time when it comes up, and again when the minute changes. Touch is not used.

The screen's light can't be turned down, so everything is drawn darker instead:
`screen_brightness = 70` (percent) in `~/.config/mptool/config.toml`, 100 for full colors.
