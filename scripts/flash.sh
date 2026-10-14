#!/usr/bin/env bash
# Build with the settings in wifi.env, flash with espflash and show the serial
# log. Variables already set in the shell win over wifi.env, so one file serves
# several boards; any arguments go to espflash, e.g. --port /dev/ttyUSB1.
#
# Usage: scripts/flash.sh [espflash options]
set -euo pipefail
cd "$(dirname "$0")/.."

# Read, never source: a value such as a URL may hold shell metacharacters.
if [ -f wifi.env ]; then
    while IFS= read -r line || [ -n "$line" ]; do
        [[ ${line%$'\r'} =~ ^([A-Z_][A-Z0-9_]*)=(.*)$ ]] || continue
        key=${BASH_REMATCH[1]}
        [ -n "${!key+set}" ] || export "$key=${BASH_REMATCH[2]}"
    done < wifi.env
else
    echo "wifi.env missing: building unprovisioned (the NO WIFI CONFIG screen)" >&2
fi
exec cargo run --release -- "$@"
