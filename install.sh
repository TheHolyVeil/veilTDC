#!/usr/bin/env bash
# veil-host installer — downloads the latest release binary from GitHub.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/TheHolyVeil/veilTDC/main/install.sh | bash
#   wget -qO- https://raw.githubusercontent.com/TheHolyVeil/veilTDC/main/install.sh | bash
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
# starter config.toml so keybinds/output/background are easy to find and edit.
# Never overwrites an existing one.
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/veil"
CONFIG_FILE="${CONFIG_DIR}/config.toml"

if [ -f "$CONFIG_FILE" ]; then
  info "config.toml already exists at ${CONFIG_FILE}, leaving it alone"
else
  bold "Fetching default config.toml..."
  mkdir -p "$CONFIG_DIR"
  CONFIG_URL="https://raw.githubusercontent.com/${REPO}/${VERSION}/config.toml"
  if curl -fsSL -o "$CONFIG_FILE" "$CONFIG_URL" 2>/dev/null || \
     wget -q -O "$CONFIG_FILE" "$CONFIG_URL" 2>/dev/null || \
     curl -fsSL -o "$CONFIG_FILE" "https://raw.githubusercontent.com/${REPO}/main/config.toml" 2>/dev/null || \
     wget -q -O "$CONFIG_FILE" "https://raw.githubusercontent.com/${REPO}/main/config.toml" 2>/dev/null; then
    info "installed : ${CONFIG_FILE}"
  else
    rm -f "$CONFIG_FILE"
    info "couldn't fetch config.toml (non-fatal) — veil-host will just use built-in defaults"
  fi
fi

# ── login manager session (dist) ──────────────────────────────────────────────
echo
reply=""
if [ -t 0 ]; then
  read -r -p "Install Veil into your login manager (Wayland desktop session)? [y/N] " reply
elif [ -e /dev/tty ]; then
  read -r -p "Install Veil into your login manager (Wayland desktop session)? [y/N] " reply < /dev/tty 2>/dev/null || true
fi

case "$reply" in
  [yY][eE][sS]|[yY])
    bold "Installing login manager session files..."
    SESSION_TMP="$(mktemp -d)"

    # Resolve veil-session (local file if running from repo, else GitHub download)
    if [ -f "./dist/veil-session" ]; then
      cp "./dist/veil-session" "${SESSION_TMP}/veil-session"
    else
      SESSION_TAG_URL="https://raw.githubusercontent.com/${REPO}/${VERSION}/dist/veil-session"
      SESSION_MAIN_URL="https://raw.githubusercontent.com/${REPO}/main/dist/veil-session"
      curl -fsSL -o "${SESSION_TMP}/veil-session" "$SESSION_TAG_URL" 2>/dev/null || \
      wget -q -O "${SESSION_TMP}/veil-session" "$SESSION_TAG_URL" 2>/dev/null || \
      curl -fsSL -o "${SESSION_TMP}/veil-session" "$SESSION_MAIN_URL" 2>/dev/null || \
      wget -q -O "${SESSION_TMP}/veil-session" "$SESSION_MAIN_URL" 2>/dev/null || true
    fi

    # Resolve veil.desktop (local file if running from repo, else GitHub download)
    if [ -f "./dist/veil.desktop" ]; then
      cp "./dist/veil.desktop" "${SESSION_TMP}/veil.desktop"
    else
      DESKTOP_TAG_URL="https://raw.githubusercontent.com/${REPO}/${VERSION}/dist/veil.desktop"
      DESKTOP_MAIN_URL="https://raw.githubusercontent.com/${REPO}/main/dist/veil.desktop"
      curl -fsSL -o "${SESSION_TMP}/veil.desktop" "$DESKTOP_TAG_URL" 2>/dev/null || \
      wget -q -O "${SESSION_TMP}/veil.desktop" "$DESKTOP_TAG_URL" 2>/dev/null || \
      curl -fsSL -o "${SESSION_TMP}/veil.desktop" "$DESKTOP_MAIN_URL" 2>/dev/null || \
      wget -q -O "${SESSION_TMP}/veil.desktop" "$DESKTOP_MAIN_URL" 2>/dev/null || true
    fi

    if [ -s "${SESSION_TMP}/veil-session" ] && [ -s "${SESSION_TMP}/veil.desktop" ]; then
      chmod 755 "${SESSION_TMP}/veil-session"
      chmod 644 "${SESSION_TMP}/veil.desktop"

      # Install veil-session to ${INSTALL_DIR}/veil-session
      if [ -w "$INSTALL_DIR" ]; then
        mv "${SESSION_TMP}/veil-session" "${INSTALL_DIR}/veil-session"
      else
        if command -v sudo &>/dev/null; then
          sudo mv "${SESSION_TMP}/veil-session" "${INSTALL_DIR}/veil-session"
        else
          info "could not install veil-session: ${INSTALL_DIR} not writable and sudo unavailable"
        fi
      fi

      # Install veil.desktop to /usr/share/wayland-sessions/veil.desktop
      WAYLAND_SESSIONS_DIR="/usr/share/wayland-sessions"
      if [ -w "$WAYLAND_SESSIONS_DIR" ] || ([ ! -d "$WAYLAND_SESSIONS_DIR" ] && [ -w "/usr/share" ]); then
        mkdir -p "$WAYLAND_SESSIONS_DIR"
        mv "${SESSION_TMP}/veil.desktop" "${WAYLAND_SESSIONS_DIR}/veil.desktop"
      else
        if command -v sudo &>/dev/null; then
          sudo mkdir -p "$WAYLAND_SESSIONS_DIR"
          sudo mv "${SESSION_TMP}/veil.desktop" "${WAYLAND_SESSIONS_DIR}/veil.desktop"
        else
          info "could not install veil.desktop: /usr/share/wayland-sessions not writable and sudo unavailable"
        fi
      fi

      info "installed : ${INSTALL_DIR}/veil-session"
      info "installed : ${WAYLAND_SESSIONS_DIR}/veil.desktop"
      green "Login manager session installed! Veil will now appear in your login manager."
    else
      info "couldn't fetch login session files (dist/); skipping login manager entry"
    fi
    rm -rf "$SESSION_TMP"
    ;;
  *)
    info "Skipping login manager session install."
    ;;
esac

green "Done. Run: veil-host probe"
