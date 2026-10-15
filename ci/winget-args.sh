#!/usr/bin/env bash
# The `winget` job's inputs (DST-R21): the published Release's Windows
# installer URL, as latest.json names it, checked against VERSION.
#
# Usage: ci/winget-args.sh LATEST_JSON VERSION
# Prints `url=<installer URL>` and `version=<version>`, for $GITHUB_OUTPUT.
set -euo pipefail
# shellcheck source=ci/lib.sh
source "$(dirname "$0")/lib.sh"

[ $# -eq 2 ] || fail "usage: $0 LATEST_JSON VERSION"
latest=$1
version=$2

listed=$(jq -r '.version // empty' "$latest" | tr -d '\r')
[ "$listed" = "$version" ] || fail "latest.json names version '$listed', not $version (DST-R21)"

url=$(jq -r '.platforms["windows-x86_64"].url // empty' "$latest" | tr -d '\r')
[[ $url == https://*"_${version}_x64-setup.exe" ]] \
  || fail "latest.json's windows-x86_64 URL '$url' isn't this version's NSIS installer (DST-R21)"

echo "url=$url"
echo "version=$version"
