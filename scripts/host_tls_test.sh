#!/usr/bin/env bash
# The firmware's PNG decoder (src/png.rs) built for the host (host/) and run
# against the PNG fixtures. No hardware involved.
#
# Usage: scripts/host_tls_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d /tmp/inkplate-tls-test.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

echo "[*] PNG decoder fixtures (synthetic + captured server output)"
python3 scripts/gen_png_fixtures.py "$WORK/png" host/fixtures/png/*.png
(cd host && cargo run --release --quiet -- png "$WORK/png")
