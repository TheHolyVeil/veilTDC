#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    echo "usage: ./pkg.sh <version>  (e.g. ./pkg.sh v1.1.0)"
    exit 1
fi

OUT="veil${VERSION}.tar.gz"

tar -czf "$OUT" \
    --exclude='*/target' \
    --exclude='*/.git' \
    README.md \
    Makefile \
    install.sh \
    pkg.sh \
    config.lua \
    Cargo.toml \
    Cargo.lock \
    crates

echo "$OUT"
