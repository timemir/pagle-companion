#!/usr/bin/env bash
# The release workflow's `check` (DST-R44): fails, before anything is built,
# unless TAG is an annotated tag v<MAJOR.MINOR.PATCH> whose version equals
# both version files at the tag, no Release of TAG exists (draft or
# published), and the version is higher than the latest Release's.
#
# Usage: ci/release-check.sh TAG RELEASES_JSON
#   run in a clone holding refs/tags/TAG as fetched from the remote;
#   RELEASES_JSON is a JSON array of every Release's {tag_name, draft, prerelease}.
# Prints the version.
set -euo pipefail
# shellcheck source=ci/lib.sh
source "$(dirname "$0")/lib.sh"

[ $# -eq 2 ] || fail "usage: $0 TAG RELEASES_JSON"
tag=$1
releases=$2

version=${tag#v}
if [ "v$version" != "$tag" ] || ! is_release_version "$version"; then
  fail "tag $tag isn't v plus MAJOR.MINOR.PATCH with no suffix and no leading zeros (DST-R40, DST-R44)"
fi

type=$(git cat-file -t "refs/tags/$tag" 2>/dev/null) || fail "tag $tag doesn't exist here (DST-R44)"
[ "$type" = tag ] || fail "tag $tag isn't an annotated tag: push it with git tag -a (DST-R44)"

conf_version=$(git show "refs/tags/$tag:src-tauri/tauri.conf.json" | jq -r '.version // empty') \
  || fail "can't read src-tauri/tauri.conf.json at $tag"
[ "$conf_version" = "$version" ] \
  || fail "src-tauri/tauri.conf.json at $tag has version '$conf_version', not $version (DST-R40, DST-R44)"

cargo_version=$(git show "refs/tags/$tag:src-tauri/Cargo.toml" | tr -d '\r' | awk '
  /^[[:space:]]*\[/ { section = $0; gsub(/[[:space:]]/, "", section) }
  section == "[package]" && /^[[:space:]]*version[[:space:]]*=/ {
    sub(/^[^=]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
  }') || fail "can't read src-tauri/Cargo.toml at $tag"
[ "$cargo_version" = "$version" ] \
  || fail "src-tauri/Cargo.toml at $tag has version '$cargo_version', not $version (DST-R40, DST-R44)"

if jq -e --arg tag "$tag" 'any(.[]; .tag_name == $tag)' "$releases" > /dev/null; then
  fail "a Release of $tag already exists, draft or published; delete a failed run's draft and the tag, then push the tag again (DST-R44)"
fi

bash "$(dirname "$0")/version-above-latest.sh" "$version" "$releases"

echo "$version"
