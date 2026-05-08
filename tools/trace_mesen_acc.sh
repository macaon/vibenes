#!/usr/bin/env bash
# Run Mesen2 in testRunner mode against AccuracyCoin (or any ROM that
# auto-runs from a Start press at the top of its menu) and produce a
# cycle-exact instruction trace on stdout.
#
# Usage:
#   tools/trace_mesen_acc.sh <rom> [limit_cycles] [start_cycles]
#                            [boot_frames] [hold_frames]
#
# Defaults match `accuracy_coin` runner conventions (boot_frames=240,
# hold_frames=8). Mesen's Lua sandbox has no `os` module, so we
# template the limits + frame counts into a temp script and launch
# Mesen against it.

set -euo pipefail

ROM="${1:?usage: $0 <rom> [limit] [start] [boot_frames] [hold_frames]}"
LIMIT="${2:-300000000}"
START="${3:-0}"
BOOT_FRAMES="${4:-240}"
HOLD_FRAMES="${5:-8}"

SCRIPT_SRC="$(dirname "$(readlink -f "$0")")/mesen_trace_acc.lua"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
TMP_LUA="$TMPDIR/trace.lua"

sed \
  -e "s/@@LIMIT_CYCLES@@/$LIMIT/" \
  -e "s/@@START_CYCLES@@/$START/" \
  -e "s/@@BOOT_FRAMES@@/$BOOT_FRAMES/" \
  -e "s/@@HOLD_FRAMES@@/$HOLD_FRAMES/" \
  "$SCRIPT_SRC" > "$TMP_LUA"

exec mesen --testRunner "$TMP_LUA" "$ROM" \
  --enableStdout --doNotSaveSettings --preferences.disableOsd=true \
  --emulation.emulationSpeed=0
