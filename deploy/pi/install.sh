#!/usr/bin/env bash
# Install sdc-client on a Raspberry Pi (64-bit OS) from GitHub releases.
# Usage: sudo ./install.sh [--version vX.Y.Z] [--server ws://host:7700/ws]
#   or:  curl -fsSL https://raw.githubusercontent.com/bcaldwell/stream-deck-controller/main/deploy/pi/install.sh | sudo bash -s -- --server ws://...
set -euo pipefail

REPO="bcaldwell/stream-deck-controller"
RAW="https://raw.githubusercontent.com/$REPO/main/deploy/pi"
VERSION="latest"
SERVER="${SDC_SERVER:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --server) SERVER="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
case "$(uname -m)" in
  aarch64|arm64) ASSET="sdc-client-aarch64-linux" ;;
  x86_64) ASSET="sdc-client-x86_64-linux" ;;
  *) echo "unsupported architecture $(uname -m); need a 64-bit OS" >&2; exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/$REPO/releases/latest/download/$ASSET"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"
fi

echo "==> downloading $URL"
tmp="$(mktemp)"
curl -fSL "$URL" -o "$tmp"
install -m 0755 "$tmp" /usr/local/bin/sdc-client
rm -f "$tmp"

# Use files next to this script when run from a checkout, otherwise fetch them.
here="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)"
fetch() { # name dest mode
  if [ -n "$here" ] && [ -f "$here/$1" ]; then install -m "$3" "$here/$1" "$2"
  else curl -fsSL "$RAW/$1" -o "$2" && chmod "$3" "$2"; fi
}
fetch 50-streamdeck.rules /etc/udev/rules.d/50-streamdeck.rules 0644
fetch sdc-client.service /etc/systemd/system/sdc-client.service 0644

if [ -z "$SERVER" ] && [ ! -f /etc/sdc-client.env ]; then
  if [ -t 0 ]; then read -r -p "Server WebSocket URL (e.g. ws://192.168.1.10:7700/ws): " SERVER
  else echo "no --server given and not interactive" >&2; exit 1; fi
fi
if [ -n "$SERVER" ]; then
  cat > /etc/sdc-client.env <<ENV
SDC_SERVER=$SERVER
SDC_BRIGHTNESS=60
RUST_LOG=info
ENV
  chmod 0644 /etc/sdc-client.env
else
  echo "==> keeping existing /etc/sdc-client.env"
fi

udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw --subsystem-match=usb || true
systemctl daemon-reload
systemctl enable sdc-client
systemctl restart sdc-client
echo "==> installed. Logs: journalctl -u sdc-client -f"
