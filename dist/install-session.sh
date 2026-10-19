#!/bin/sh
# Install the Veil session entry. Needs root. Run from the repo's dist/ dir:
#   sudo ./install-session.sh [path/to/veil-host]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
host=${1:-$here/../target/release/veil-host}
[ -x "$host" ] || { echo "veil-host binary not found: $host (build with: cargo build --release)"; exit 1; }
install -Dm755 "$host" /usr/local/bin/veil-host
install -Dm755 "$here/veil-session" /usr/local/bin/veil-session
install -Dm644 "$here/veil.desktop" /usr/share/wayland-sessions/veil.desktop
echo "Installed. Log out and pick \"Veil\" in velogin."
echo "If it drops back to the greeter: ~/.local/state/veil/veil-host.log"
