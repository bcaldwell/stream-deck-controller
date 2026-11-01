#!/bin/sh
# Install sdc-client on a Raspberry Pi (64-bit Linux). Run ON THE PI as root, with the binary,
# 50-streamdeck.rules and sdc-client.service next to this script. `task pi:install` copies those
# files over from your Mac and runs this under sudo; see deploy/pi/README.md.
#
#   sudo sh install.sh --server wss://streamdeck.example.com/ws --token-file ./token
#
# Options:
#   --server URL       WebSocket URL of the server. Optional on re-runs: the value in
#                      /etc/sdc-client.env is kept.
#   --token TOKEN      Shared client token (server's SDC_CLIENT_TOKEN). Visible in `ps`; prefer
#                      --token-file. Optional on re-runs: the existing token is kept.
#   --token-file FILE  Read the token from FILE and delete FILE afterwards.
#   --binary FILE      sdc-client binary (default: ./sdc-client next to this script)
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SERVER=""
TOKEN=""
TOKEN_FILE=""
BINARY="$HERE/sdc-client"
DIR=/var/lib/sdc-client
ENV_FILE=/etc/sdc-client.env

while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER="${2:?--server needs a value}"; shift 2 ;;
    --token) TOKEN="${2:?--token needs a value}"; shift 2 ;;
    --token-file) TOKEN_FILE="${2:?--token-file needs a value}"; shift 2 ;;
    --binary) BINARY="${2:?--binary needs a value}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
for f in "$BINARY" "$HERE/50-streamdeck.rules" "$HERE/sdc-client.service"; do
  [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
done

if [ -n "$TOKEN_FILE" ]; then
  TOKEN="$(tr -d '\r\n' < "$TOKEN_FILE")"
  rm -f "$TOKEN_FILE"
fi
# systemd reads the env file itself, so keep the value free of anything it would reinterpret.
case "$TOKEN" in
  *[!A-Za-z0-9._~+/=-]*) echo "token may only contain A-Za-z0-9 . _ ~ + / = -" >&2; exit 1 ;;
esac

# Re-runs keep what is already configured unless overridden.
env_get() { [ -f "$ENV_FILE" ] && sed -n "s/^$1=//p" "$ENV_FILE" | head -n 1 || true; }
if [ -z "$SERVER" ]; then
  SERVER="$(env_get SDC_SERVER)"
  [ -z "$SERVER" ] || echo "==> keeping server $SERVER from $ENV_FILE"
fi
[ -n "$SERVER" ] || { echo "missing --server wss://host/ws" >&2; exit 1; }
if [ -z "$TOKEN" ]; then
  TOKEN="$(env_get SDC_CLIENT_TOKEN)"
  [ -z "$TOKEN" ] || echo "==> keeping token from $ENV_FILE"
fi
[ -n "$TOKEN" ] || echo "==> no token set: the server must not require one" >&2

# The service runs as a dedicated user that owns $DIR so the client can replace its own
# binary when the server pushes an update (DynamicUser would hide the dir under
# /var/lib/private and make that ownership awkward).
id sdc >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin sdc
systemctl stop sdc-client 2>/dev/null || true
install -d -o sdc -g sdc -m 0755 "$DIR"
install -o sdc -g sdc -m 0755 "$BINARY" "$DIR/sdc-client"

install -m 0644 "$HERE/50-streamdeck.rules" /etc/udev/rules.d/50-streamdeck.rules
install -m 0644 "$HERE/sdc-client.service" /etc/systemd/system/sdc-client.service

# Root-only: systemd (PID 1) reads EnvironmentFile, the unprivileged service never does.
# Settings other than the two managed here (SDC_BRIGHTNESS, SDC_CLIENT_ID, ...) are kept.
new="$(umask 077 && mktemp "$ENV_FILE.XXXXXX")"
{
  echo "SDC_SERVER=$SERVER"
  [ -z "$TOKEN" ] || echo "SDC_CLIENT_TOKEN=$TOKEN"
  if [ -f "$ENV_FILE" ]; then
    grep -v -e '^SDC_SERVER=' -e '^SDC_CLIENT_TOKEN=' "$ENV_FILE" || true
  else
    echo "SDC_BRIGHTNESS=60"
    echo "RUST_LOG=info"
  fi
} > "$new"
chown root:root "$new"
chmod 0600 "$new"
mv "$new" "$ENV_FILE"

# The Pi has no RTC and sits on a network without internet access; the firewall allows it
# NTP to this one host only. A drifting clock eventually fails TLS against a renewed cert.
mkdir -p /etc/systemd/timesyncd.conf.d
printf '[Time]\nNTP=time.cloudflare.com\n' > /etc/systemd/timesyncd.conf.d/sdc.conf
systemctl restart systemd-timesyncd 2>/dev/null || true

udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw --subsystem-match=usb || true
systemctl daemon-reload
systemctl enable sdc-client
systemctl restart sdc-client
echo "==> installed. Logs: journalctl -u sdc-client -f"
