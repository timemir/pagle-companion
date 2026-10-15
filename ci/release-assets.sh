#!/usr/bin/env bash
# `publish` step 1 (DST-R46): fails unless the draft Release holds exactly
# the four assets of DST-R41 other than latest.json, all fully uploaded:
# the Windows installer, its .sig, the AppImage and its .sig (Contracts,
# "Release assets"). A latest.json left by an earlier attempt is ignored.
#
# Usage: ci/release-assets.sh RELEASE_JSON VERSION
#   RELEASE_JSON is the Release as GitHub's REST API returns it.
# Prints `installer=<name>` and `appimage=<name>`, for $GITHUB_OUTPUT.
set -euo pipefail
# shellcheck source=ci/lib.sh
source "$(dirname "$0")/lib.sh"

[ $# -eq 2 ] || fail "usage: $0 RELEASE_JSON VERSION"
release=$1
version=$2

names=$(jq -r '.assets[] | select(.name != "latest.json") | .name' "$release" | tr -d '\r')
count=$(jq '[.assets[] | select(.name != "latest.json")] | length' "$release" | tr -d '\r')

pick() { # the one asset name ending in $1, or fail
  local found
  found=$(jq -r --arg end "$1" '[.assets[].name | select(endswith($end))] | if length == 1 then .[0] else length end' "$release" | tr -d '\r')
  [[ $found == *"$1" ]] || fail "the draft has $found assets ending in $1, not 1 (DST-R41, DST-R46)"
  echo "$found"
}

installer=$(pick "_${version}_x64-setup.exe")
appimage=$(pick "_${version}_amd64.AppImage")

for name in "$installer" "$installer.sig" "$appimage" "$appimage.sig"; do
  state=$(jq -r --arg name "$name" '.assets[] | select(.name == $name) | .state' "$release" | tr -d '\r')
  [ -n "$state" ] || fail "the draft has no asset $name (DST-R41, DST-R46)"
  [ "$state" = uploaded ] || fail "asset $name is '$state', not uploaded (DST-R46)"
done

if [ "$count" != 4 ]; then
  fail "the draft has $count assets besides latest.json, not 4: $(tr '\n' ' ' <<< "$names")(DST-R41, DST-R46)"
fi

echo "installer=$installer"
echo "appimage=$appimage"
