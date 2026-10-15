#!/usr/bin/env bash
# Prints the message of annotated tag TAG, without a signature block or
# trailing newlines: the Release body and latest.json's notes (DST-R41).
#
# Usage: ci/tag-notes.sh TAG   (in a clone holding refs/tags/TAG)
set -euo pipefail
# shellcheck source=ci/lib.sh
source "$(dirname "$0")/lib.sh"

[ $# -eq 1 ] || fail "usage: $0 TAG"
tag=$1

type=$(git cat-file -t "refs/tags/$tag" 2>/dev/null) || fail "tag $tag doesn't exist here"
[ "$type" = tag ] || fail "tag $tag isn't an annotated tag, so it has no message (DST-R41)"

# A tag object is headers, a blank line, then the message; a signed tag
# appends its signature to the message.
notes=$(git cat-file tag "refs/tags/$tag" \
  | sed '1,/^$/d' \
  | sed '/^-----BEGIN \(PGP SIGNATURE\|SSH SIGNATURE\|SIGNED MESSAGE\)-----$/,$d')
printf '%s' "$notes"
