#!/usr/bin/env bash
# The firmware's shared code (src/http.rs, src/png.rs) built for the host
# (host/) and run against a local HTTP test server and the PNG fixtures. No
# hardware involved.
#
# Usage: scripts/host_tls_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d /tmp/inkplate-tls-test.XXXXXX)
PLAIN_PORT=47334
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

echo "[*] Starting server (http:$PLAIN_PORT)"
serve --port "$PLAIN_PORT"

for port in $PLAIN_PORT; do
    for _ in $(seq 1 50); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then exec 3>&-; break; fi
        sleep 0.1
    done
done

echo "[*] Building + running host scenarios"
(cd host && cargo run --release --quiet -- "$PLAIN_PORT" "$SIZE" "$TOKEN")

echo "[*] PNG decoder fixtures (synthetic + captured server output)"
python3 scripts/gen_png_fixtures.py "$WORK/png" host/fixtures/png/*.png
(cd host && cargo run --release --quiet -- png "$WORK/png")
