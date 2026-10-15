#!/usr/bin/env bash
# Prints every Release of this repository, drafts included (the token can
# push), as a JSON array of {tag_name, draft, prerelease}: the input of
# release-check.sh and version-above-latest.sh. Needs GH_TOKEN.
set -euo pipefail
gh api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100" \
  --jq '.[] | {tag_name, draft, prerelease}' | jq -s .
