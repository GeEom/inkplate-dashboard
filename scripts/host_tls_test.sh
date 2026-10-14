#!/usr/bin/env bash
# The firmware's shared code (src/http.rs, src/png.rs, src/roots.rs) built for
# the host (host/) and run against local TLS 1.3 and HTTP test servers, the
# live Makespace endpoint and the PNG fixtures. No hardware involved.
#
# Usage: scripts/host_tls_test.sh
#   The live scenarios run when wifi.env has TEST_MAKESPACE_TOKEN (members-app's
#   EINK_DISPLAY_TOKEN) or MS_TOKEN is set; LIVE=0 skips them. MS_HOST and
#   MS_PATH pick the live target.
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d /tmp/inkplate-tls-test.XXXXXX)
GOOD_PORT=47331 EVIL_PORT=47332 PLAIN_PORT=47334
SIZE=100000 TOKEN=testtok
PIDS=()

cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT

serve() {
    python3 scripts/tls_test_server.py --token "$TOKEN" --size "$SIZE" "$@" &
    PIDS+=($!)
}

echo "[*] Minting test PKIs (good + evil twin, same names)"
./scripts/gen_tls_cert.sh inkplate-test "$WORK/good"
./scripts/gen_tls_cert.sh inkplate-test "$WORK/evil"

echo "[*] Starting servers (https good:$GOOD_PORT, evil:$EVIL_PORT, http:$PLAIN_PORT)"
serve --port "$GOOD_PORT" --cert "$WORK/good/server.pem" --key "$WORK/good/server.key.pem"
serve --port "$EVIL_PORT" --cert "$WORK/evil/server.pem" --key "$WORK/evil/server.key.pem"
serve --port "$PLAIN_PORT"

for port in $GOOD_PORT $EVIL_PORT $PLAIN_PORT; do
    for _ in $(seq 1 50); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then exec 3>&-; break; fi
        sleep 0.1
    done
done

echo "[*] Building + running host scenarios"
LIVE=${LIVE:-1}
[ "$LIVE" = 1 ] || MS_HOST=
MS_HOST=${MS_HOST-app.makespace.org}
MS_PATH=${MS_PATH-/equipment/wood-shop-band-saw/trouble-tickets.png?width=1280&height=720}
MS_TOKEN=${MS_TOKEN-$(grep "^TEST_MAKESPACE_TOKEN=" wifi.env 2>/dev/null | cut -d= -f2- || true)}
if [ -n "$MS_HOST" ] && [ -n "$MS_TOKEN" ]; then export MS_HOST MS_PATH MS_TOKEN; fi
(cd host && cargo run --release --quiet -- \
    "$GOOD_PORT" "$EVIL_PORT" "$PLAIN_PORT" "$WORK/good/ca.der" "$SIZE" "$TOKEN")

echo "[*] Unit tests (the root store's pinned hashes)"
(cd host && cargo test --release --quiet)

echo "[*] PNG decoder fixtures (synthetic + captured server output)"
python3 scripts/gen_png_fixtures.py "$WORK/png" host/fixtures/png/*.png
(cd host && cargo run --release --quiet -- png "$WORK/png")
