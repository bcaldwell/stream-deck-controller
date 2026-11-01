# Raspberry Pi Zero 2 W client setup

The client is a single static aarch64 binary, so any 64-bit Raspberry Pi OS works.
Use **Raspberry Pi OS Lite (64-bit)**.

1. Flash with Raspberry Pi Imager: choose *Raspberry Pi Zero 2 W*, OS *Raspberry Pi OS Lite (64-bit)*.
   In the settings (gear / "Edit settings") set hostname (e.g. `deck-kitchen`), user + password,
   Wi-Fi credentials, and enable SSH.
2. Plug the Stream Deck into the Pi's data USB port (the one labelled USB, not PWR) using an OTG adapter.
   Boot the Pi and `ssh <user>@deck-kitchen.local`.
3. Install (replace the URL with your server; `--version v0.2.0` pins a release, default is latest):

   ```sh
   curl -fsSL https://raw.githubusercontent.com/bcaldwell/stream-deck-controller/main/deploy/pi/install.sh \
     | sudo bash -s -- --server ws://192.168.1.10:7700/ws
   ```

   Or from a checkout: `sudo deploy/pi/install.sh --server ws://...`. The script installs
   `/usr/local/bin/sdc-client`, the udev rule, the systemd unit, writes `/etc/sdc-client.env`
   and starts the service.
4. Check: `journalctl -u sdc-client -f`. Change settings in `/etc/sdc-client.env`
   (`SDC_SERVER`, `SDC_CLIENT_ID`, `SDC_BRIGHTNESS`, `RUST_LOG`), then `sudo systemctl restart sdc-client`.

Unplugging/replugging the deck or restarting the server is handled automatically.
The binary is downloaded from the GitHub release assets `sdc-client-aarch64-linux`.
