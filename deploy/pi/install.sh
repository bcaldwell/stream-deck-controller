#!/bin/sh
# Install sdc-client on a 64-bit Linux box (Raspberry Pi OS Lite) from an sdc-server.
# The server hands out its own client binary, so the Pi needs no internet access.
#
#   curl -fsSL http://<server>:7700/install.sh | sudo sh -s -- --server ws://<server>:7700/ws
#
# Options:
#   --server URL   WebSocket URL of the server (or $SDC_SERVER). Optional when re-running
#                  on a box that already has /etc/sdc-client.env.
#   --base URL     HTTP base to download from (default: derived from --server)
#
# The udev rule and systemd unit below mirror 50-streamdeck.rules and sdc-client.service
# (the server tests check they stay identical).
set -eu

SERVER="${SDC_SERVER:-}"
BASE=""
DIR=/var/lib/sdc-client

while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER="${2:?--server needs a value}"; shift 2 ;;
    --base) BASE="${2:?--base needs a value}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }

if [ -z "$SERVER" ] && [ -f /etc/sdc-client.env ]; then
  SERVER="$(sed -n 's/^SDC_SERVER=//p' /etc/sdc-client.env | head -n 1)"
  [ -z "$SERVER" ] || echo "==> keeping server $SERVER from /etc/sdc-client.env"
fi
[ -n "$SERVER" ] || { echo "missing --server ws://host:7700/ws" >&2; exit 1; }
if [ -z "$BASE" ]; then
  # ws://host:port/ws -> http://host:port, wss:// -> https://
  BASE="$(printf '%s' "$SERVER" | sed -e 's#^ws://#http://#' -e 's#^wss://#https://#' -e 's#^\(https\{0,1\}://[^/]*\).*#\1#')"
fi
BASE="${BASE%/}"

case "$(uname -m)" in
  aarch64|arm64) TARGET=aarch64-linux ;;
  x86_64) TARGET=x86_64-linux ;;
  *) echo "unsupported architecture $(uname -m); need a 64-bit OS" >&2; exit 1 ;;
esac

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  echo "need curl or wget" >&2; exit 1
fi
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "==> downloading $BASE/client/$TARGET"
fetch "$BASE/client/$TARGET" "$tmp/sdc-client"
fetch "$BASE/client/$TARGET.sha256" "$tmp/sha256"
want="$(tr -d ' \n\r' < "$tmp/sha256")"
got="$(sha256 "$tmp/sdc-client")"
[ "$want" = "$got" ] || { echo "checksum mismatch: want $want, got $got" >&2; exit 1; }

# The service runs as a dedicated user that owns $DIR so the client can replace its own
# binary when the server pushes an update (DynamicUser would hide the dir under
# /var/lib/private and make that ownership awkward).
id sdc >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin sdc
systemctl stop sdc-client 2>/dev/null || true
install -d -o sdc -g sdc -m 0755 "$DIR"
install -o sdc -g sdc -m 0755 "$tmp/sdc-client" "$DIR/sdc-client"

cat > /etc/udev/rules.d/50-streamdeck.rules <<'EOF'
# Allow non-root access to Elgato Stream Decks (vendor 0fd9) via hidraw/usb.
KERNEL=="hidraw*", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="0fd9", MODE="0666"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0fd9", MODE="0666"
EOF

cat > /etc/systemd/system/sdc-client.service <<'EOF'
[Unit]
Description=Stream Deck Controller client
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=/etc/sdc-client.env
ExecStart=/var/lib/sdc-client/sdc-client
# The client exits after replacing itself with a newer binary from the server.
Restart=always
RestartSec=3
# Generic, low-privilege hardening (device access is granted by the udev rule).
# StateDirectory is the only writable path: it holds the self-updating binary.
User=sdc
Group=sdc
StateDirectory=sdc-client
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes

[Install]
WantedBy=multi-user.target
EOF

cat > /etc/sdc-client.env <<EOF
SDC_SERVER=$SERVER
SDC_BRIGHTNESS=60
RUST_LOG=info
EOF
chmod 0644 /etc/sdc-client.env

udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw --subsystem-match=usb || true
systemctl daemon-reload
systemctl enable sdc-client
systemctl restart sdc-client
echo "==> installed. Logs: journalctl -u sdc-client -f"
