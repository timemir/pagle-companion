#!/usr/bin/env bash
# Unit tests for the release scripts in ci/, run without GitHub against
# fake tags, Releases and a throwaway minisign key pair. Each test names the
# rule it checks. Needs git, jq and minisign 0.10 or newer.
#
# Usage: bash ci/test.sh
set -uo pipefail

ci=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
passed=0
failed=0

for tool in git jq minisign base64; do
  command -v "$tool" > /dev/null || { echo "ci/test.sh needs $tool" >&2; exit 2; }
done

ok() { passed=$((passed + 1)); echo "ok - $1"; }
bad() {
  failed=$((failed + 1))
  echo "not ok - $1"
  [ -z "${2:-}" ] || while IFS= read -r line; do echo "    | $line"; done <<< "$2"
}

# expect_ok NAME COMMAND...: COMMAND succeeds; its stdout is left in $out.
expect_ok() {
  local name=$1; shift
  if out=$("$@" 2> "$work/stderr"); then ok "$name"; else bad "$name" "$(cat "$work/stderr")"; fi
}

# expect_fail NAME MESSAGE COMMAND...: COMMAND fails, saying MESSAGE.
expect_fail() {
  local name=$1 message=$2; shift 2
  if out=$("$@" 2> "$work/stderr"); then
    bad "$name: passed, should fail" "$out"
  elif grep -qF -- "$message" "$work/stderr"; then
    ok "$name"
  else
    bad "$name: failed without '$message'" "$(cat "$work/stderr")"
  fi
}

# expect_eq NAME ACTUAL EXPECTED
expect_eq() {
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected: $3"$'\n'"actual:   $2"; fi
}

# --- Fixtures ----------------------------------------------------------------

repo=$work/repo
git init -q "$repo"
git -C "$repo" config user.name Test
git -C "$repo" config user.email test@example.com
git -C "$repo" config commit.gpgsign false
git -C "$repo" config tag.gpgsign false
git -C "$repo" config core.autocrlf false
mkdir -p "$repo/src-tauri"

# commit_versions CONF_VERSION CARGO_VERSION: commits both version files.
commit_versions() {
  printf '{\n  "productName": "Pagle Companion",\n  "version": "%s"\n}\n' "$1" \
    > "$repo/src-tauri/tauri.conf.json"
  cat > "$repo/src-tauri/Cargo.toml" <<TOML
[package]
name = "pagle-companion"
version = "$2"
edition = "2024"

[dependencies]
serde = { version = "1", features = ["derive"] }

[dependencies.tauri]
version = "2"
TOML
  git -C "$repo" add -A
  git -C "$repo" commit -q -m "versions $1 $2"
}

check() { (cd "$repo" && bash "$ci/release-check.sh" "$@"); }
notes() { (cd "$repo" && bash "$ci/tag-notes.sh" "$@"); }

# releases FILE TAG:STATE...: a Release list, STATE being published, draft
# or prerelease.
releases() {
  local file=$work/$1 entry; shift
  {
    echo '['
    local first=1
    for entry in "$@"; do
      [ $first = 1 ] || echo ','
      first=0
      local tag=${entry%%:*} state=${entry#*:}
      printf '{"tag_name":"%s","draft":%s,"prerelease":%s}' "$tag" \
        "$([ "$state" = draft ] && echo true || echo false)" \
        "$([ "$state" = prerelease ] && echo true || echo false)"
    done
    echo ']'
  } > "$file"
  echo "$file"
}

none=$(releases none.json)
up_to_123=$(releases up-to-123.json v0.9.0:published v1.2.2:published v1.2.3:published)
draft_124=$(releases draft-124.json v1.2.3:published v1.2.4:draft)
published_124=$(releases published-124.json v1.2.3:published v1.2.4:published)
up_to_130=$(releases up-to-130.json v1.2.3:published v1.3.0:published)
ignored=$(releases ignored.json v1.2.3:published v9.0.0:prerelease v2.0.0:draft nightly:published v3.0:published)
up_to_1_10=$(releases up-to-1-10.json v1.9.0:published v1.10.0:published)

commit_versions 1.2.4 1.2.4
git -C "$repo" tag -a v1.2.4 -m "Fixes the tray icon.

- Uploads resume after sleep.
- Notes keep their blank lines."
commit_versions 1.2.5 1.2.5
git -C "$repo" tag v1.2.5 # lightweight
commit_versions 1.3.1 1.3.0
git -C "$repo" tag -a v1.3.0 -m "conf differs"
commit_versions 1.4.0 1.3.9
git -C "$repo" tag -a v1.4.0 -m "cargo differs"
commit_versions 0.1.0 0.1.0
git -C "$repo" tag -a v0.1.0 -m "First Release"

# A signed tag: its signature is part of the stored message.
signed=$(printf 'object %s\ntype commit\ntag v0.1.1\ntagger Test <test@example.com> 1700000000 +0000\n\nSigned notes\n-----BEGIN PGP SIGNATURE-----\n\niQEzBAABCAAdFiEE\n-----END PGP SIGNATURE-----\n' \
  "$(git -C "$repo" rev-parse HEAD)" | git -C "$repo" mktag)
git -C "$repo" update-ref refs/tags/v0.1.1 "$signed"

# --- check (DST-R44) ---------------------------------------------------------

expect_ok "DST-R44: v1.2.4 over the latest Release 1.2.3 passes" check v1.2.4 "$up_to_123"
expect_eq "DST-R44: check prints the version" "$out" "1.2.4"
expect_ok "DST-R44: the first ever Release passes" check v0.1.0 "$none"
expect_ok "DST-R44/R42: drafts, prereleases and non-version tags aren't the latest Release" check v1.2.4 "$ignored"

expect_fail "DST-R44: a lightweight tag fails" "isn't an annotated tag" check v1.2.5 "$up_to_123"
expect_fail "DST-R44: a tag missing here fails" "doesn't exist" check v7.7.7 "$up_to_123"
for tag in v1.2 v1.2.3-rc.1 v01.2.3 v1.02.3 v1.2.03 1.2.4 v1.2.3.4 v1.2.3+build.1 V1.2.4 "v 1.2.4" v1.2.4x; do
  expect_fail "DST-R40/R44: tag '$tag' fails" "isn't v plus MAJOR.MINOR.PATCH" check "$tag" "$none"
done
expect_fail "DST-R40/R44: tauri.conf.json's version differing from the tag fails" "tauri.conf.json at v1.3.0 has version '1.3.1'" check v1.3.0 "$up_to_123"
expect_fail "DST-R40/R44: Cargo.toml's version differing from the tag fails" "Cargo.toml at v1.4.0 has version '1.3.9'" check v1.4.0 "$up_to_123"
expect_fail "DST-R44/R90: an existing draft of the tag fails" "already exists" check v1.2.4 "$draft_124"
expect_fail "DST-R44/R90: an existing published Release of the tag fails" "already exists" check v1.2.4 "$published_124"
expect_fail "DST-R44: a version not above the latest Release fails" "isn't higher than the latest Release's, 1.3.0" check v1.2.4 "$up_to_130"

# --- SemVer ordering against the latest Release (DST-R44, DST-R46 step 4) ----

above() { bash "$ci/version-above-latest.sh" "$@"; }
for pair in 1.2.4:1.2.3 1.10.0:1.9.0 2.0.0:1.99.99 1.2.10:1.2.9 10.0.0:9.9.9 1.3.0:1.2.99 12345678901234567890.0.0:9.0.0; do
  list=$(releases "one.json" "v${pair#*:}:published")
  expect_ok "DST-R44: ${pair%%:*} is above ${pair#*:}" above "${pair%%:*}" "$list"
done
for pair in 1.2.3:1.2.3 1.2.2:1.2.3 1.9.0:1.10.0 1.99.99:2.0.0 0.9.9:1.0.0; do
  list=$(releases "one.json" "v${pair#*:}:published")
  expect_fail "DST-R44: ${pair%%:*} isn't above ${pair#*:}" "isn't higher" above "${pair%%:*}" "$list"
done
expect_fail "DST-R44: the latest Release is the highest published version, not the last listed" "isn't higher than the latest Release's, 1.10.0" above 1.9.5 "$up_to_1_10"
expect_ok "DST-R46 step 4: the run's own draft doesn't count as the latest Release" above 1.2.4 "$draft_124"
expect_fail "DST-R46 step 4: a Release published meanwhile at or above the version fails" "isn't higher" above 1.2.4 "$published_124"
expect_ok "DST-R44: no Release yet passes" above 0.0.1 "$none"

# --- Release notes from the tag (DST-R41) ------------------------------------

expect_ok "DST-R41: the notes are the annotated tag's message" notes v1.2.4
expect_eq "DST-R41: the notes keep the whole message, blank lines included" "$out" "Fixes the tray icon.

- Uploads resume after sleep.
- Notes keep their blank lines."
expect_ok "DST-R41: a signed tag's notes" notes v0.1.1
expect_eq "DST-R41: a signed tag's notes leave out the signature" "$out" "Signed notes"
expect_fail "DST-R41: a lightweight tag has no notes" "isn't an annotated tag" notes v1.2.5

# --- The draft's assets (DST-R41, DST-R46 step 1) ----------------------------

# release_json FILE STATE NAME...: a Release with these assets, all in STATE.
release_json() {
  local file=$work/$1 state=$2; shift 2
  local name list=()
  for name in "$@"; do
    list+=("$(jq -cn --arg name "$name" --arg state "$state" '{
      id: 1, name: $name, label: $name, state: $state,
      browser_download_url: ("https://github.com/timemir/pagle-companion/releases/download/untagged-0123abcd/" + $name)
    }')")
  done
  printf '%s\n' "${list[@]}" | jq -s '{id: 42, tag_name: "v1.2.4", draft: true, assets: .}' > "$file"
  echo "$file"
}

exe=Pagle.Companion_1.2.4_x64-setup.exe
app=Pagle.Companion_1.2.4_amd64.AppImage
four=$(release_json four.json uploaded "$exe" "$exe.sig" "$app" "$app.sig")
with_old_latest=$(release_json old-latest.json uploaded "$exe" "$exe.sig" "$app" "$app.sig" latest.json)

assets() { bash "$ci/release-assets.sh" "$@"; }
expect_ok "DST-R46 step 1: exactly the four assets pass" assets "$four" 1.2.4
expect_eq "DST-R46 step 1: the installer and AppImage are named" "$out" "installer=$exe
appimage=$app"
expect_ok "DST-R46 step 1: a latest.json from an earlier attempt is ignored" assets "$with_old_latest" 1.2.4
expect_fail "DST-R46 step 1: a missing .sig fails" "no asset $app.sig" \
  assets "$(release_json no-sig.json uploaded "$exe" "$exe.sig" "$app")" 1.2.4
expect_fail "DST-R46 step 1: a missing AppImage fails" "assets ending in _1.2.4_amd64.AppImage, not 1" \
  assets "$(release_json no-app.json uploaded "$exe" "$exe.sig" "$app.sig")" 1.2.4
expect_fail "DST-R41/R46 step 1: a fifth asset fails" "has 5 assets besides latest.json, not 4" \
  assets "$(release_json extra.json uploaded "$exe" "$exe.sig" "$app" "$app.sig" pagle-companion.exe)" 1.2.4
expect_fail "DST-R46 step 1: an installer of another version fails" "assets ending in _1.2.4_x64-setup.exe, not 1" \
  assets "$(release_json other-version.json uploaded Pagle.Companion_1.2.3_x64-setup.exe Pagle.Companion_1.2.3_x64-setup.exe.sig "$app" "$app.sig")" 1.2.4
expect_fail "DST-R46 step 1: a half-uploaded asset fails" "is 'new', not uploaded" \
  assets "$(release_json half.json new "$exe" "$exe.sig" "$app" "$app.sig")" 1.2.4

# --- Updater signatures (DST-R25, DST-R46 step 2) ----------------------------

keys=$work/keys
mkdir -p "$keys"
minisign -G -W -p "$keys/a.pub" -s "$keys/a.key" > /dev/null
minisign -G -W -p "$keys/b.pub" -s "$keys/b.key" > /dev/null
printf 'installer bytes\n' > "$keys/$exe"
cp "$keys/$exe" "$keys/tampered.exe"
printf 'one more byte' >> "$keys/tampered.exe"
minisign -S -s "$keys/a.key" -m "$keys/$exe" -x "$keys/a.minisig" -t "file:$exe" > /dev/null
minisign -S -s "$keys/b.key" -m "$keys/$exe" -x "$keys/b.minisig" -t "file:$exe" > /dev/null
# Tauri wraps the minisign files in base64, the public key in tauri.conf.json too.
base64 -w0 < "$keys/a.minisig" > "$keys/a.sig"
base64 -w0 < "$keys/b.minisig" > "$keys/b.sig"
pubkey=$(base64 -w0 < "$keys/a.pub")
printf '!!! not base64 !!!' > "$keys/garbage.sig"

verify() { bash "$ci/verify-sig.sh" "$@"; }
expect_ok "DST-R25: a .sig made with the updater key verifies" verify "$pubkey" "$keys/$exe" "$keys/a.sig"
expect_fail "DST-R25: a .sig made with another key fails" "doesn't verify" verify "$pubkey" "$keys/$exe" "$keys/b.sig"
expect_fail "DST-R25: a changed file fails" "doesn't verify" verify "$pubkey" "$keys/tampered.exe" "$keys/a.sig"
expect_fail "DST-R25: a .sig that isn't base64 fails" "isn't base64" verify "$pubkey" "$keys/$exe" "$keys/garbage.sig"
expect_fail "DST-R25: a missing .sig fails" "doesn't exist" verify "$pubkey" "$keys/$exe" "$keys/none.sig"
expect_fail "DST-R25: another public key fails" "doesn't verify" \
  verify "$(base64 -w0 < "$keys/b.pub")" "$keys/$exe" "$keys/a.sig"

real_pubkey=$(jq -r .plugins.updater.pubkey "$ci/../src-tauri/tauri.conf.json" | tr -d '\r')
expect_eq "DST-R47: tauri.conf.json's pubkey is a base64 minisign public key" \
  "$(printf '%s' "$real_pubkey" | base64 -d | head -n 1 | cut -c 1-38)" "untrusted comment: minisign public key"

# --- latest.json (DST-R46 step 3, Contracts "Release assets") ----------------

sigs=$work/sigs
mkdir -p "$sigs"
printf 'V0lORE9XUyBTSUc=\n' > "$sigs/$exe.sig"
printf 'TElOVVggU0lH' > "$sigs/$app.sig"
notes v1.2.4 > "$work/notes.md"

latest() { bash "$ci/latest-json.sh" "$@"; }
expect_ok "DST-R46 step 3: latest.json is written" latest "$with_old_latest" 1.2.4 v1.2.4 "$work/notes.md" "$sigs"
printf '%s' "$out" > "$work/latest.json"
lj() { jq -r "$@" "$work/latest.json" | tr -d '\r'; }
expect_eq "DST-R46: latest.json has exactly version, notes, pub_date and platforms" "$(lj -c 'keys')" '["notes","platforms","pub_date","version"]'
expect_eq "DST-R46: latest.json has exactly the two platform keys" "$(lj -c '.platforms | keys')" '["linux-x86_64","windows-x86_64"]'
expect_eq "DST-R46: each platform has exactly signature and url" "$(lj -c '[.platforms[] | keys] | unique')" '[["signature","url"]]'
expect_eq "DST-R46: version is the tag's" "$(lj .version)" "1.2.4"
expect_eq "DST-R41/R46: notes are the tag message" "$(lj .notes)" "$(notes v1.2.4)"
expect_eq "DST-R46: pub_date is UTC, to the second" \
  "$(lj '.pub_date | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")')" true
expect_eq "DST-R46: pub_date is now" "$(lj '(.pub_date | fromdate) - now | fabs < 120')" true
expect_eq "DST-R46: the Windows URL is the attached installer's, under the tag" "$(lj '.platforms["windows-x86_64"].url')" \
  "https://github.com/timemir/pagle-companion/releases/download/v1.2.4/$exe"
expect_eq "DST-R46: the Linux URL is the attached AppImage's, under the tag" "$(lj '.platforms["linux-x86_64"].url')" \
  "https://github.com/timemir/pagle-companion/releases/download/v1.2.4/$app"
expect_eq "DST-R46: the Windows signature is the installer's .sig content" "$(lj '.platforms["windows-x86_64"].signature')" "V0lORE9XUyBTSUc="
expect_eq "DST-R46: the Linux signature is the AppImage's .sig content" "$(lj '.platforms["linux-x86_64"].signature')" "TElOVVggU0lH"

rm "$sigs/$app.sig"
expect_fail "DST-R46 step 3: a missing .sig fails" "$app.sig is missing" latest "$four" 1.2.4 v1.2.4 "$work/notes.md" "$work/sigs"
printf 'TElOVVggU0lH' > "$sigs/$app.sig"
jq '.assets[0].browser_download_url = "https://example.com/elsewhere"' "$four" > "$work/odd-url.json"
expect_fail "DST-R46 step 3: an asset URL of an unexpected shape fails" "unexpected download URL" \
  latest "$work/odd-url.json" 1.2.4 v1.2.4 "$work/notes.md" "$sigs"
expect_fail "DST-R46 step 3: latest.json isn't written for a draft without its four assets" "no asset $app.sig" \
  latest "$(release_json no-sig.json uploaded "$exe" "$exe.sig" "$app")" 1.2.4 v1.2.4 "$work/notes.md" "$sigs"

# --- winget inputs (DST-R21) --------------------------------------------------

winget() { bash "$ci/winget-args.sh" "$@"; }
expect_ok "DST-R21: winget takes the installer URL from latest.json" winget "$work/latest.json" 1.2.4
expect_eq "DST-R21: winget gets the Release's installer URL and version" "$out" \
  "url=https://github.com/timemir/pagle-companion/releases/download/v1.2.4/$exe
version=1.2.4"
expect_fail "DST-R21: a latest.json of another version fails" "names version '1.2.4', not 1.2.5" winget "$work/latest.json" 1.2.5

echo
echo "$passed passed, $failed failed"
[ "$failed" = 0 ]
