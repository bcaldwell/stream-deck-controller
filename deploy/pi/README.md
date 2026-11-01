# Raspberry Pi Zero 2 W client setup

The client is a single static aarch64 binary, so any 64-bit Raspberry Pi OS works.
Use **Raspberry Pi OS Lite (64-bit)**. The Pi is isolated: it only needs to reach the server over
the network, and it never needs internet access. The install is pushed to it from your Mac.

1. Flash with Raspberry Pi Imager: choose *Raspberry Pi Zero 2 W*, OS *Raspberry Pi OS Lite (64-bit)*.
   In the settings (gear / "Edit settings") set hostname (e.g. `deck-kitchen`), user + password,
   Wi-Fi credentials, and enable SSH.
2. Plug the Stream Deck into the Pi's data USB port (the one labelled USB, not PWR) using an OTG adapter.
   Boot the Pi and make sure you can `ssh <user>@deck-kitchen.local`.
3. From the repo on your Mac (inside `nix develop`), with the server's client token in the environment:

   ```sh
   export SDC_CLIENT_TOKEN=...   # same value as the server's SDC_CLIENT_TOKEN
   task pi:install HOST=pi@192.168.2.81 SERVER=wss://streamdeck.bcaldwell.co/ws
   ```

   `HOST` and `SERVER` default to those values. The task builds the static aarch64 client, copies it
   with `install.sh`, `50-streamdeck.rules` and `sdc-client.service` to a temp dir on the Pi, and runs
   `sudo sh install.sh ...` over one interactive ssh session (one sudo password prompt). The token is
   sent over ssh stdin into a 0600 file that the script deletes, so it never shows up in `ps`.
   Leave `SDC_CLIENT_TOKEN` unset on a re-run to keep the token already on the Pi.

   `install.sh` (runs on the Pi as root) creates the `sdc` user, installs the binary to
   `/var/lib/sdc-client/sdc-client`, the udev rule and the systemd unit, writes `/etc/sdc-client.env`
   (mode 0600, root-only; systemd reads it) with `SDC_SERVER` and `SDC_CLIENT_TOKEN`, and (re)starts
   the service. Options: `--server URL`, `--token-file FILE` (or `--token TOKEN`, visible in `ps`),
   `--binary FILE`; on re-runs omitted `--server`/`--token` keep the existing values.
4. Check: `journalctl -u sdc-client -f`. Change settings in `/etc/sdc-client.env`
   (`SDC_SERVER`, `SDC_CLIENT_TOKEN`, `SDC_CLIENT_ID`, `SDC_BRIGHTNESS`, `SDC_NO_UPDATE`, `RUST_LOG`), then
   `sudo systemctl restart sdc-client`. A log line "server rejected token" means the token is wrong.

Unplugging/replugging the deck or restarting the server is handled automatically.

## Updates

Deploying a new server image is the update. On connect the client reports the sha256 of its binary; if the
server has a different one it sends the new path, the client downloads it (with the same bearer token), verifies the hash, replaces
`/var/lib/sdc-client/sdc-client` atomically and exits, and systemd (`Restart=always`) starts the new version.
Set `SDC_NO_UPDATE=1` in `/etc/sdc-client.env` to pin a build. If the binary directory is not writable the
client logs a warning and keeps running the old version.

The service runs as a dedicated system user `sdc` (created by the installer) that owns
`/var/lib/sdc-client`, rather than `DynamicUser`, so the binary has a stable path and owner it can replace.
Hardening (`ProtectSystem=strict`, `NoNewPrivileges`, `ProtectHome`) is kept; `StateDirectory` is the only
writable path. The udev rule and unit installed are the files in this directory.

A binary pushed with `task pi:install` is replaced on the next connect if the server serves a different
build, unless `SDC_NO_UPDATE=1` is set in `/etc/sdc-client.env`.
