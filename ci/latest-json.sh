#!/usr/bin/env bash
# `publish` step 3 (DST-R46): writes latest.json (DST Contracts, "Release
# assets") to stdout: the version, the tag message as notes, pub_date (now,
# UTC) and exactly the platforms windows-x86_64 and linux-x86_64, each with
# its .sig content and the download URL of the asset actually attached.
#
# A draft's asset URLs name an `untagged-…` placeholder instead of the tag
# until it is published, so the tag is put in its place.
#
# Usage: ci/latest-json.sh RELEASE_JSON VERSION TAG NOTES_FILE SIG_DIR
#   RELEASE_JSON is the draft Release as GitHub's REST API returns it;
#   SIG_DIR holds the two assets' .sig files under their asset names.
set -euo pipefail
here=$(dirname "$0")
# shellcheck source=ci/lib.sh
source "$here/lib.sh"

[ $# -eq 5 ] || fail "usage: $0 RELEASE_JSON VERSION TAG NOTES_FILE SIG_DIR"
release=$1
version=$2
tag=$3
notes=$4
sigs=$5

assets=$(bash "$here/release-assets.sh" "$release" "$version")
installer=$(sed -n 's/^installer=//p' <<< "$assets")
appimage=$(sed -n 's/^appimage=//p' <<< "$assets")

for name in "$installer" "$appimage"; do
  [ -s "$sigs/$name.sig" ] || fail "$sigs/$name.sig is missing or empty (DST-R46)"
done

jq -n \
  --arg version "$version" \
  --arg tag "$tag" \
  --rawfile notes "$notes" \
  --arg installer "$installer" \
  --arg appimage "$appimage" \
  --rawfile installer_sig "$sigs/$installer.sig" \
  --rawfile appimage_sig "$sigs/$appimage.sig" \
  --slurpfile release "$release" '
  def url($name):
    ($release[0].assets[] | select(.name == $name) | .browser_download_url) as $url
    | ($url | capture("^(?<base>https://[^/]+/[^/]+/[^/]+/releases/download/)[^/]+/(?<file>[^/]+)$"))
    // error("asset \($name) has an unexpected download URL: \($url)")
    | "\(.base)\($tag)/\(.file)";
  def trim: sub("\\s+$"; "");
  {
    version: $version,
    notes: $notes,
    pub_date: (now | floor | todate),
    platforms: {
      "windows-x86_64": { signature: ($installer_sig | trim), url: url($installer) },
      "linux-x86_64":   { signature: ($appimage_sig | trim), url: url($appimage) }
    }
  }' || fail "can't write latest.json (DST-R46)"
