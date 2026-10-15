# Helpers shared by the release scripts (DST-R40 to R46). Sourced, not run.
# shellcheck shell=bash

# Prints an error (a GitHub annotation inside Actions) and exits.
fail() {
  if [ "${GITHUB_ACTIONS:-}" = true ]; then
    echo "::error::$*" >&2
  else
    echo "error: $*" >&2
  fi
  exit 1
}

# DST-R40: MAJOR.MINOR.PATCH, no pre-release or build suffix, no leading zeros.
is_release_version() {
  [[ $1 =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
}

# Compares two non-negative integers of any length without overflowing:
# prints -1, 0 or 1.
num_cmp() {
  if [ "${#1}" -ne "${#2}" ]; then
    [ "${#1}" -lt "${#2}" ] && echo -1 || echo 1
  elif [ "$1" = "$2" ]; then
    echo 0
  else
    [[ $1 < $2 ]] && echo -1 || echo 1
  fi
}

# Succeeds when release version $1 is higher than release version $2 by
# SemVer 2.0.0 precedence (DST-R44, DST-R46).
version_gt() {
  local a b i c
  IFS=. read -r -a a <<< "$1"
  IFS=. read -r -a b <<< "$2"
  for i in 0 1 2; do
    c=$(num_cmp "${a[$i]}" "${b[$i]}")
    [ "$c" = 1 ] && return 0
    [ "$c" = -1 ] && return 1
  done
  return 1
}

# Prints the version of the latest Release (DST-R42: the highest published,
# non-prerelease Release whose tag is v<release version>), or nothing before
# the first one. $1 is a JSON array of {tag_name, draft, prerelease}.
latest_release_version() {
  local tag version latest=""
  while IFS= read -r tag; do
    tag=${tag%$'\r'}
    version=${tag#v}
    if [ "v$version" != "$tag" ] || ! is_release_version "$version"; then continue; fi
    if [ -z "$latest" ] || version_gt "$version" "$latest"; then
      latest=$version
    fi
  done < <(jq -r '.[] | select((.draft | not) and (.prerelease | not)) | .tag_name' "$1")
  printf '%s' "$latest"
}
