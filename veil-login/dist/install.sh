#!/usr/bin/env sh
set -eu

REPO="${REPO:-viewerofall/veilTDC}"
VERSION="${VERSION:-latest}"

if [ "$(id -u)" -ne 0 ]; then
    echo "[velogin] run this installer as root" >&2
    exit 1
fi

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "[velogin] missing required command: $1" >&2
        exit 1
    fi
}

fetch() {
    url=$1
    out=$2
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" -o "$out"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$out" "$url"
    else
        echo "[velogin] need curl or wget to download release assets" >&2
        exit 1
    fi
}

require_file() {
    if [ ! -f "$1" ]; then
        echo "[velogin] missing required file: $1" >&2
        exit 1
    fi
}

resolve_version() {
    if [ "$VERSION" != "latest" ]; then
        printf '%s\n' "$VERSION"
        return
    fi

    need_cmd sed
    tmp=$(mktemp)
    fetch "https://api.github.com/repos/${REPO}/releases/latest" "$tmp"
    version=$(sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp" | head -n 1)
    rm -f "$tmp"
    if [ -z "$version" ]; then
        echo "[velogin] could not resolve latest release tag" >&2
        exit 1
    fi
    printf '%s\n' "$version"
}

download_bundle() {
    need_cmd mktemp
    need_cmd tar
    TMPDIR_VELOGIN=$(mktemp -d)
    trap 'rm -rf "$TMPDIR_VELOGIN"' EXIT INT TERM

    version=$(resolve_version)
    archive="$TMPDIR_VELOGIN/velogin.tar.gz"
    fetch "https://github.com/${REPO}/releases/download/${version}/velogin.tar.gz" "$archive"
    tar -xzf "$archive" -C "$TMPDIR_VELOGIN"
}

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DIST_DIR="$SCRIPT_DIR/dist"
BIN_SRC="$SCRIPT_DIR/velogin"
PAM_SRC="$DIST_DIR/pam.d/velogin"
SERVICE_SRC="$DIST_DIR/velogin.service"

if [ ! -f "$BIN_SRC" ] || [ ! -f "$PAM_SRC" ] || [ ! -f "$SERVICE_SRC" ]; then
    download_bundle
    SCRIPT_DIR="$TMPDIR_VELOGIN/velogin"
    DIST_DIR="$SCRIPT_DIR/dist"
    BIN_SRC="$SCRIPT_DIR/velogin"
    PAM_SRC="$DIST_DIR/pam.d/velogin"
    SERVICE_SRC="$DIST_DIR/velogin.service"
fi

require_file "$BIN_SRC"
require_file "$PAM_SRC"
require_file "$SERVICE_SRC"

install -Dm755 "$BIN_SRC" /usr/local/bin/velogin
install -Dm644 "$PAM_SRC" /etc/pam.d/velogin
install -Dm644 "$SERVICE_SRC" /etc/systemd/system/velogin.service

systemctl daemon-reload

cat <<EOF
[velogin] installed:
  /usr/local/bin/velogin
  /etc/pam.d/velogin
  /etc/systemd/system/velogin.service

[velogin] next:
  systemctl disable getty@tty1.service
  systemctl enable velogin.service

[velogin] then reboot, or start velogin manually after disabling tty1 getty.
EOF
