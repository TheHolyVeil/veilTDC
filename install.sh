#!/usr/bin/env bash
# veil-host installer — downloads the latest release binary from GitHub.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/viewerofall/veilTDC/main/install.sh | bash
#   wget -qO- https://raw.githubusercontent.com/viewerofall/veilTDC/main/install.sh | bash
#
# Options (env vars):
#   INSTALL_DIR   — where to install (default: /usr/local/bin)
#   VERSION       — pin a specific tag (default: latest)

set -euo pipefail

REPO="TheHolyVeil/veilTDC"
BIN="veil-host"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
VERSION="${VERSION:-}"

# ── colour helpers ────────────────────────────────────────────────────────────
red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'  "$*"; }
info()  { printf '  %s\n' "$*"; }

die() { red "error: $*"; exit 1; }

# ── detect platform ───────────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

[ "$OS" = "Linux" ] || die "veil-host only supports Linux (got $OS)"

case "$ARCH" in
  x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
  *)       die "unsupported architecture: $ARCH" ;;
esac

# ── resolve version ───────────────────────────────────────────────────────────
API="https://api.github.com/repos/${REPO}/releases"

if [ -z "$VERSION" ]; then
  bold "Fetching latest release..."
  VERSION="$(curl -fsSL "${API}/latest" | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
  [ -n "$VERSION" ] || die "could not resolve latest release (is the repo public?)"
fi

info "repo    : https://github.com/${REPO}"
info "version : ${VERSION}"
info "target  : ${TARGET}"
info "install : ${INSTALL_DIR}/${BIN}"
echo

# ── download ──────────────────────────────────────────────────────────────────
ASSET="${BIN}-${TARGET}"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

bold "Downloading ${ASSET}..."
if command -v curl &>/dev/null; then
  curl -fsSL --progress-bar -o "${TMP}/${BIN}" "$URL" \
    || die "download failed — check that release ${VERSION} has asset ${ASSET}"
elif command -v wget &>/dev/null; then
  wget -q --show-progress -O "${TMP}/${BIN}" "$URL" \
    || die "download failed — check that release ${VERSION} has asset ${ASSET}"
else
  die "neither curl nor wget found"
fi

chmod +x "${TMP}/${BIN}"

# ── install ───────────────────────────────────────────────────────────────────
bold "Installing to ${INSTALL_DIR}/${BIN}..."

if [ -w "$INSTALL_DIR" ]; then
  mv "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
else
  if command -v sudo &>/dev/null; then
    sudo mv "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
  else
    die "${INSTALL_DIR} is not writable and sudo is unavailable. Re-run as root or set INSTALL_DIR to a writable path."
  fi
fi

# ── default config ────────────────────────────────────────────────────────────
# Not required — veil-host runs fine with built-in defaults — but drop a
# starter config.lua so keybinds/output/background are easy to find and edit.
# Never overwrites an existing one.
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/veil"
CONFIG_FILE="${CONFIG_DIR}/config.lua"

if [ -f "$CONFIG_FILE" ]; then
  info "config.lua already exists at ${CONFIG_FILE}, leaving it alone"
else
  bold "Fetching default config.lua..."
  mkdir -p "$CONFIG_DIR"
  CONFIG_URL="https://raw.githubusercontent.com/${REPO}/${VERSION}/config.lua"
  if curl -fsSL -o "$CONFIG_FILE" "$CONFIG_URL" 2>/dev/null || wget -q -O "$CONFIG_FILE" "$CONFIG_URL" 2>/dev/null; then
    info "installed : ${CONFIG_FILE}"
  else
    rm -f "$CONFIG_FILE"
    info "couldn't fetch config.lua (non-fatal) — veil-host will just use built-in defaults"
  fi
fi

green "Done. Run: veil-host probe"
