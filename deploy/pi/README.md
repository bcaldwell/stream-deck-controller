# Raspberry Pi Zero 2 W client setup

The client is a single static aarch64 binary, so any 64-bit Raspberry Pi OS works.
Use **Raspberry Pi OS Lite (64-bit)**. The Pi only needs to reach the server: the server hands out
the client binary and the installer, so no internet access is required.

1. Flash with Raspberry Pi Imager: choose *Raspberry Pi Zero 2 W*, OS *Raspberry Pi OS Lite (64-bit)*.
   In the settings (gear / "Edit settings") set hostname (e.g. `deck-kitchen`), user + password,
   Wi-Fi credentials, and enable SSH.
2. Plug the Stream Deck into the Pi's data USB port (the one labelled USB, not PWR) using an OTG adapter.
   Boot the Pi and `ssh <user>@deck-kitchen.local`.
3. Install from your server (replace the host; needs `curl` or `wget`):

   ```sh
   curl -fsSL http://192.168.1.10:7700/install.sh | sudo sh -s -- --server ws://192.168.1.10:7700/ws
   ```

   The script downloads `/client/aarch64-linux` (checking its `.sha256`) from the server and installs
   `/var/lib/sdc-client/sdc-client`, the udev rule, the systemd unit and `/etc/sdc-client.env`, then
   starts the service. `--base http://host:port` overrides the download URL (default: derived from `--server`).
4. Check: `journalctl -u sdc-client -f`. Change settings in `/etc/sdc-client.env`
   (`SDC_SERVER`, `SDC_CLIENT_ID`, `SDC_BRIGHTNESS`, `SDC_NO_UPDATE`, `RUST_LOG`), then `sudo systemctl restart sdc-client`.

Unplugging/replugging the deck or restarting the server is handled automatically.

## Updates

Deploying a new server image is the update. On connect the client reports the sha256 of its binary; if the
server has a different one it sends the new path, the client downloads it, verifies the hash, replaces
`/var/lib/sdc-client/sdc-client` atomically and exits, and systemd (`Restart=always`) starts the new version.
Set `SDC_NO_UPDATE=1` in `/etc/sdc-client.env` to pin a build. If the binary directory is not writable the
client logs a warning and keeps running the old version.

The service runs as a dedicated system user `sdc` (created by the installer) that owns
`/var/lib/sdc-client`, rather than `DynamicUser`, so the binary has a stable path and owner it can replace.
Hardening (`ProtectSystem=strict`, `NoNewPrivileges`, `ProtectHome`) is kept; `StateDirectory` is the only
writable path. The udev rule and unit are embedded in `install.sh`; `50-streamdeck.rules` and
`sdc-client.service` here are the readable copies, and a server test fails if they drift apart.

To try a locally built binary on a Pi, use `task deploy-pi` (the server will overwrite it on the next
connect unless it serves the same build or `SDC_NO_UPDATE` is set).
