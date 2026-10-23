#!/usr/bin/env bash
# `publish` step 2 (DST-R25, DST-R46): fails unless FILE's Tauri updater
# signature SIG verifies with minisign against PUBKEY, the value of
# `plugins.updater.pubkey` in tauri.conf.json. Both are base64 of minisign
# files, a public key and a signature, so they are decoded first.
#
# Then fails unless the signature is for VERSION and for FILE's kind of
# bundle. latest.json isn't signed, so the Companion only installs a file
# whose signed trusted comment names the version latest.json announces and
# a bundle of its own platform (DST-R52, DST-R53): `tauri signer` writes
# `timestamp:<t>\tfile:<bundle name>`, and the bundle's name is
# `<product>_<version>_<arch and suffix>`, such as `_x64-setup.exe` or
# `_amd64.AppImage`. A Release whose signatures every Companion would refuse
# never goes out.
#
# Usage: ci/verify-sig.sh PUBKEY FILE SIG VERSION
set -euo pipefail
# shellcheck source=ci/lib.sh
source "$(dirname "$0")/lib.sh"

[ $# -eq 4 ] || fail "usage: $0 PUBKEY FILE SIG VERSION"
pubkey=$1
file=$2
sig=$3
version=$4

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

# The trusted comment is signed, and only read once the signature verified.
comment=$(sed -n 's/^trusted comment: //p' "$tmp/asset.minisig" | tr -d '\r')
signed_file=
while IFS= read -r field; do
  case $field in
    version:*)
      signed=${field#version:}
      [ "${signed#v}" = "$version" ] \
        || fail "$(basename "$sig") is signed for version '${signed#v}', not $version (DST-R52)"
      ;;
    file:*) signed_file=${field#file:} ;;
  esac
done < <(tr '\t' '\n' <<< "$comment")
[ -n "$signed_file" ] || fail "$(basename "$sig")'s trusted comment names no file (DST-R52)"
# The version sits between the name's last two underscores.
signed=$(sed -nE 's/^.+_([^_]+)_[^_]+$/\1/p' <<< "$signed_file")
[ -n "$signed" ] || fail "$(basename "$sig") is for '$signed_file', a name without a version (DST-R52)"
[ "$signed" = "$version" ] \
  || fail "$(basename "$sig") is signed for version '$signed', not $version (DST-R52)"
asset=$(basename "$file")
[ "${signed_file##*_}" = "${asset##*_}" ] \
  || fail "$(basename "$sig") is signed as '$signed_file', another kind of bundle than $asset (DST-R53)"
echo "verified $asset for $version"
