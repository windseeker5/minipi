# Mini Pi

A Raspberry Pi with a big drive that backs up the Minipass server every night.
It runs Arch Linux ARM (no desktop) and **mpbackup**, the backup tool in `backup/`.

Works on a Raspberry Pi 3 or 4 (64-bit). Not on a Pi 5.

## Recipe

All commands run on the laptop, from this folder.

**1. Make the SD card** (plug the card into the laptop):

```bash
sudo ./flash.sh
```

**2. Start the Pi.** Put the card in, plug in the network cable, then the power. Wait a minute.

**3. Set it up** (finds the Pi by itself, takes 20 to 40 minutes):

```bash
./install.sh
```

It asks for a username and a password for the Pi, then does the rest:
updates Arch, builds the backup tool, copies your backup settings
(`~/.config/mpbackup/config.toml`) and lets the Pi log in to the Minipass server.

**4. Add the big drive** (plug it into the Pi, with its own power if it has a plug):

```bash
ssh -t minipi sudo ~/minipi/pi/add-drive.sh
```

Done: a backup runs every night at 3:00, and Discord tells you how it went.

## Every day

| What | Command |
|------|---------|
| Open the backup menu | `ssh -t minipi mpbackup` |
| Run a backup now | `ssh minipi mpbackup run` |
| Get the newest backup tool from GitHub | `ssh minipi '~/minipi/pi/update.sh'` |

If `ssh minipi` doesn't work, use the Pi's address (install.sh shows it), e.g. `ssh 192.168.1.50`.

## What's in here

| File | Runs on | Does |
|------|---------|------|
| `flash.sh` | laptop | Puts Arch on the SD card, with your SSH key on it |
| `install.sh` | laptop | Finds the Pi and sets it all up over the network |
| `pi/setup.sh` | Pi | Updates Arch, creates your user, installs the tool |
| `pi/update.sh` | Pi | Pulls the latest tool from GitHub and rebuilds it |
| `pi/add-drive.sh` | Pi | Prepares the big drive and starts the nightly backup |
| `backup/` | | The backup tool (Rust) |

Secrets (the Discord link, server keys) never go in this repo. They live only in
`~/.config/mpbackup/config.toml` on the laptop and on the Pi.

## Later

The LCD screen: a logo animation and the backup status.
