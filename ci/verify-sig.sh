#!/usr/bin/env bash
# `publish` step 2 (DST-R25, DST-R46): fails unless FILE's Tauri updater
# signature SIG verifies with minisign against PUBKEY, the value of
# `plugins.updater.pubkey` in tauri.conf.json. Both are base64 of minisign
# files, a public key and a signature, so they are decoded first.
#
# Usage: ci/verify-sig.sh PUBKEY FILE SIG
set -euo pipefail
# shellcheck source=ci/lib.sh
source "$(dirname "$0")/lib.sh"

[ $# -eq 3 ] || fail "usage: $0 PUBKEY FILE SIG"
pubkey=$1
file=$2
sig=$3

command -v minisign > /dev/null || fail "minisign isn't installed"
[ -f "$file" ] || fail "$file doesn't exist"
[ -f "$sig" ] || fail "$sig doesn't exist"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

printf '%s' "$pubkey" | base64 -d > "$tmp/updater.pub" 2> /dev/null \
  || fail "the updater public key isn't base64 (DST-R47)"
tr -d '[:space:]' < "$sig" | base64 -d > "$tmp/asset.minisig" 2> /dev/null \
  || fail "$(basename "$sig") isn't base64 (DST-R25)"

minisign -V -q -p "$tmp/updater.pub" -x "$tmp/asset.minisig" -m "$file" \
  || fail "$(basename "$sig") doesn't verify $(basename "$file") against the updater public key (DST-R25)"
echo "verified $(basename "$file")"
