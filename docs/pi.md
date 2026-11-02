# Raspberry Pi deck

A Raspberry Pi Zero 2 W with a Stream Deck on its USB port makes a cheap, always-on physical deck. The Pi runs
`sdc-client`, a single static aarch64 binary; the server does all the work. The Pi never needs internet access, only
a route to the server. The full installer notes are in [deploy/pi/README.md](../deploy/pi/README.md).

## Setup

1. **Flash the SD card** with Raspberry Pi Imager: device *Raspberry Pi Zero 2 W*, OS *Raspberry Pi OS Lite
   (64-bit)*. In the settings set a hostname (for example `deck-kitchen`), a user and password, Wi-Fi, and enable SSH.
2. **Plug in the deck** to the Pi's data USB port (the one labelled USB, not PWR) through an OTG adapter. Boot and
   check that `ssh <user>@deck-kitchen.local` works.
3. **Install from your computer**, inside `nix develop`, with the server's client token in the environment:

   ```sh
   export SDC_CLIENT_TOKEN=...      # same value as the server's SDC_CLIENT_TOKEN
   task pi:install HOST=pi@deck-kitchen.local SERVER=wss://your-server/ws
   ```

   The task builds the aarch64 client, copies it to the Pi with the installer, udev rule and systemd unit, and
   installs everything over one ssh session (one sudo prompt). The token travels over ssh stdin into a root-only
   file, never on a command line.

4. **Check**: `journalctl -u sdc-client -f` on the Pi. "server rejected token" means the token is wrong.

### Keeping settings in `.env`

Values can live in a gitignored `.env` at the repository root instead of on the command line (Taskfile loads it):

```sh
SDC_PI_HOST=pi@deck-kitchen.local
SDC_SERVER=wss://your-server/ws
SDC_CLIENT_TOKEN=...
```

Then it is just `task pi:install`. Leave `SDC_CLIENT_TOKEN` unset on a re-run to keep the token already on the Pi.

## Configuring the client

On the Pi, settings are in `/etc/sdc-client.env` (root-only, read by systemd). After editing run
`sudo systemctl restart sdc-client`.

| Variable | Meaning |
|----------|---------|
| `SDC_SERVER` | Server websocket URL, e.g. `wss://your-server/ws` |
| `SDC_CLIENT_TOKEN` | Shared secret matching the server |
| `SDC_CLIENT_ID` | Name the server knows this deck by (default: hostname). Pages are remembered per id |
| `SDC_BRIGHTNESS` | Fallback brightness (60); the server's `sdc.brightness` overrides it |
| `SDC_OFFLINE_AFTER` | Seconds without a server connection before the deck shows its dimmed offline screen (1800). Until then it keeps the last images and a press flashes a red-barred "offline" on the key |
| `SDC_USB_SUSPEND` | `false` disables USB suspend (see below) |
| `SDC_NO_UPDATE` | Set to `1` to pin this build and ignore updates |
| `RUST_LOG` | Log filter |

Unplugging and replugging the deck, or restarting the server, is handled automatically.

## Power saving

Two layers keep a battery-powered Pi cheap to run, both driven by `sdc.idle` in the config
([Idle](configuration.md#idle-dimming-and-power)):

- **Dim and off**: the server lowers the brightness, then sets it to 0.
- **USB suspend**: when the screen has been off for about a second, the client closes the device so nothing holds
  it open. The udev rule (`50-streamdeck.rules`) sets `power/control=auto` and `autosuspend_delay_ms=2000`, so the
  kernel suspends the deck two seconds later. The images are kept. The client then only polls
  `/sys/bus/usb/devices/*/power/runtime_status`, which does not wake the deck.

A key press wakes the deck itself; the client reopens it, redraws anything that changed in the meantime and tells the
server, which restores the brightness and restarts the idle timers. The press that woke the deck is not delivered as
a key press, the same as for any dimmed deck.

Notes:

- A Pi installed before this feature needs `task pi:install` run once more to get the udev rule. Without it the deck
  never suspends; the client notices after 15 s, logs a warning and keeps the device open.
- Linux only. If the deck's sysfs entry cannot be found, the device is simply never released.
- To turn it off set `SDC_USB_SUSPEND=false` in `/etc/sdc-client.env`.
- The Pi's own power draw (Wi-Fi, CPU) is not reduced by this; only the deck is.

## Updates

Deploying a new server image updates every client. On connect the client reports the sha256 of its binary; if the
server has a different build it sends the path, the client downloads it with the same token, verifies the hash,
replaces itself and exits, and systemd restarts it. `task pi:install` is needed only for the first install (and to
refresh the udev rule or unit). Set `SDC_NO_UPDATE=1` to pin a build.

The service runs as a dedicated `sdc` system user that owns `/var/lib/sdc-client`, with hardening
(`ProtectSystem=strict`, `NoNewPrivileges`, `ProtectHome`).

## Without hardware

`task run-fake-client` runs the client with no deck: frames are written to `tmp/fake/key-N.jpg` and you type
`press 3` on stdin. `task list-devices` shows which decks are detected on the machine.
