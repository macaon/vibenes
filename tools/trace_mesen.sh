#!/usr/bin/env bash
# Run Mesen2 in testRunner mode against a ROM and produce a cycle-exact
# instruction trace on stdout.
#
# Usage: tools/trace_mesen.sh <rom> [limit_cycles] [start_cycles]
# Defaults: limit_cycles=300000, start_cycles=0
#
# Mesen's Lua sandbox has no `os` module, so we template the limits into a
# temp script and launch Mesen against it.

set -euo pipefail

ROM="${1:?usage: $0 <rom> [limit_cycles] [start_cycles]}"
LIMIT="${2:-300000}"
START="${3:-0}"

SCRIPT_SRC="$(dirname "$(readlink -f "$0")")/mesen_trace.lua"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
TMP_LUA="$TMPDIR/trace.lua"

sed \
  -e "s/@@LIMIT_CYCLES@@/$LIMIT/" \
  -e "s/@@START_CYCLES@@/$START/" \
  "$SCRIPT_SRC" > "$TMP_LUA"

exec mesen --testRunner "$TMP_LUA" "$ROM" \
  --enableStdout --doNotSaveSettings --preferences.disableOsd=true \
  --emulation.emulationSpeed=0
