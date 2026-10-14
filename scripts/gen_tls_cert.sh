#!/usr/bin/env bash
# Mint a test PKI for the host test servers, in the shape the firmware's
# verifier accepts (P-256, ECDSA-with-SHA256, the name in the leaf's CN):
#   ca.der                      the root to trust
#   server.pem, server.key.pem  <name>, issued by an intermediate CA
#
# Usage: gen_tls_cert.sh <name> <outdir>
set -euo pipefail

NAME="${1:?usage: gen_tls_cert.sh <name> <outdir>}"
OUT="${2:?usage: gen_tls_cert.sh <name> <outdir>}"
mkdir -p "$OUT"
cd "$OUT"

# issue <file> <cn> <issuer> <CA:TRUE|CA:FALSE>
issue() {
    openssl ecparam -name prime256v1 -genkey -noout -out "$1.key.pem"
    openssl req -new -key "$1.key.pem" -subj "/CN=$2" |
        openssl x509 -req -sha256 -days 36500 -CA "$3.crt.pem" -CAkey "$3.key.pem" \
            -extfile <(echo "basicConstraints=critical,$4") -out "$1.crt.pem" 2>/dev/null
}

openssl ecparam -name prime256v1 -genkey -noout -out ca.key.pem
openssl req -new -x509 -sha256 -key ca.key.pem -subj "/CN=$NAME root" -days 36500 -out ca.crt.pem
openssl x509 -in ca.crt.pem -outform der -out ca.der
issue inter "$NAME intermediate" ca CA:TRUE
issue server "$NAME" inter CA:FALSE
cat server.crt.pem inter.crt.pem > server.pem
