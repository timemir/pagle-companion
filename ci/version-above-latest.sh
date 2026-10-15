#!/usr/bin/env bash
# Fails unless VERSION is higher than the latest Release's version, or there
# is no Release yet. Run by `check` (DST-R44) and again by `publish` just
# before it publishes (DST-R46 step 4).
#
# Usage: ci/version-above-latest.sh VERSION RELEASES_JSON
#   RELEASES_JSON is a JSON array of every Release's {tag_name, draft, prerelease}.
set -euo pipefail
# shellcheck source=ci/lib.sh
source "$(dirname "$0")/lib.sh"

[ $# -eq 2 ] || fail "usage: $0 VERSION RELEASES_JSON"
version=$1
releases=$2

is_release_version "$version" || fail "$version isn't MAJOR.MINOR.PATCH (DST-R40)"
latest=$(latest_release_version "$releases")
if [ -n "$latest" ] && ! version_gt "$version" "$latest"; then
  fail "version $version isn't higher than the latest Release's, $latest (DST-R44, DST-R46)"
fi
