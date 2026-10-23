# Security

## Reporting a vulnerability

Please don't open a public issue. Report it privately through [GitHub's private vulnerability reporting](https://github.com/timemir/pagle-companion/security/advisories/new), with the steps to reproduce it and the version of Pagle Companion. A fix goes out as a new Release, which every Companion installs on its own; a security fix may also raise the minimum Companion version, so that older versions stop uploading until they update ([DST-R65](https://github.com/timemir/pagle-backend/blob/main/docs/spec/companion/distribution.md#version-gate)).

Anything that lets a server, a file on disk, another program or another user of the same computer make the Companion do what it shouldn't is in scope: the updater, the credential, what it writes into the game's `Interface/AddOns` folder, its window. A problem with the Pagle website can be reported the same way.

## Checking a download

Releases aren't Authenticode-signed ([ADR 0010](https://github.com/timemir/pagle-backend/blob/main/docs/adr/0010-the-companion-ships-unsigned-through-free-channels.md)). Every Windows installer and AppImage is signed with the updater key instead, which the Companion also checks before it installs any update. To check a download yourself with [minisign](https://jedisct1.github.io/minisign/), download its `.sig` from the same Release too, then, in a clone of this repository:

```sh
jq -r .plugins.updater.pubkey src-tauri/tauri.conf.json | base64 -d > pagle-companion.pub
base64 -d < "Pagle.Companion_1.2.3_x64-setup.exe.sig" > download.minisig
minisign -V -p pagle-companion.pub -x download.minisig -m "Pagle.Companion_1.2.3_x64-setup.exe"
```

`Signature and comment signature verified` means the file is the one the release workflow built, and the trusted comment it prints names the file and its version.
