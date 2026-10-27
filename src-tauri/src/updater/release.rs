//! What the updater reads and how it trusts it: the endpoint and public key
//! from `tauri.conf.json`'s `plugins.updater`, read as the updater plugin's
//! own [`Config`] (DST Contracts, "Tauri configuration"); `latest.json`, read
//! as the plugin's own [`RemoteRelease`] (DST Contracts, "Release assets");
//! the offer rule (DST-R52); and the signature check (DST-R53), the plugin's
//! steps with the plugin's verifier, `minisign-verify`.
//!
//! The plugin's `Updater` and its verification need a Tauri `AppHandle`
//! with the plugin registered, and headless mode has no Tauri app (CMP-R1);
//! so the updater uses the plugin's types and verifier directly, in both
//! modes alike.

use std::cmp::Ordering;

use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use tauri_plugin_updater::{Config, RemoteRelease};

/// The variable a debug build takes another endpoint from, for a local test
/// server, as `PAGLE_COMPANION_BASE_URL` does for the backend (CMP
/// Contracts, HTTP). Release builds use only the configured endpoint.
pub const ENDPOINT_VAR: &str = "PAGLE_COMPANION_UPDATE_URL";

const CONF: &str = include_str!("../../tauri.conf.json");

/// `plugins.updater` from `tauri.conf.json`, as the plugin reads it.
///
/// # Panics
///
/// If it isn't the plugin's configuration, which the tests rule out.
pub fn plugin_config() -> Config {
    let conf: serde_json::Value = serde_json::from_str(CONF).expect("tauri.conf.json is JSON");
    serde_json::from_value(conf["plugins"]["updater"].clone())
        .expect("plugins.updater is the updater plugin's configuration")
}

/// The endpoint this build checks: the single configured one (DST-R50), or
/// `PAGLE_COMPANION_UPDATE_URL` in a debug build when it is set.
pub fn endpoint(config: &Config) -> String {
    resolve_endpoint(
        cfg!(debug_assertions),
        std::env::var(ENDPOINT_VAR).ok(),
        config,
    )
}

fn resolve_endpoint(debug_build: bool, from_env: Option<String>, config: &Config) -> String {
    match from_env {
        Some(url) if debug_build && !url.trim().is_empty() => url.trim().to_owned(),
        _ => config
            .endpoints
            .first()
            .map(|url| url.to_string())
            .unwrap_or_default(),
    }
}

/// This build's key in `latest.json`'s `platforms`: `windows-x86_64` or
/// `linux-x86_64` (DST Contracts, "Release assets").
pub fn platform() -> String {
    tauri_plugin_updater::target().unwrap_or_default()
}

/// Whether `url` names a file of this Companion's Releases: one under the
/// folder of `endpoint`, the URL that serves `latest.json`. On GitHub that
/// folder is the repository's `releases/`, which serves `latest.json` at
/// `releases/latest/download/latest.json` and each file at
/// `releases/download/<tag>/<file>`. Both are compared as parsed URLs, with
/// `.` and `..` segments resolved, on the same scheme, host and port and
/// without a user name, so `latest.json` can't point the download anywhere
/// else. The signature (DST-R53) is what makes a file trusted; this only
/// keeps the updater from fetching what no Release holds.
pub fn is_release_file(endpoint: &str, url: &str) -> bool {
    let (Ok(endpoint), Ok(url)) = (reqwest::Url::parse(endpoint), reqwest::Url::parse(url)) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() || url.origin() != endpoint.origin() {
        return false;
    }
    let path = endpoint.path();
    let folder = &path[..path.rfind('/').map_or(0, |i| i + 1)];
    let folder = folder.strip_suffix("latest/download/").unwrap_or(folder);
    url.path().len() > folder.len() && url.path().starts_with(folder)
}

/// What `latest.json` says about the latest Release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latest {
    pub version: Version,
    /// This platform's file, if `latest.json` names one.
    pub asset: Option<Asset>,
}

/// A file of the Release and its updater signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub url: String,
    /// The `.sig` file's content: base64 of a minisign signature.
    pub signature: String,
}

/// Reads `latest.json`, Tauri's static update format. `None` for a bad
/// document, which counts as a failed check (DST-R51).
pub fn read_latest(body: &[u8], platform: &str) -> Option<Latest> {
    let release: RemoteRelease = serde_json::from_slice(body).ok()?;
    let asset = match (release.download_url(platform), release.signature(platform)) {
        (Ok(url), Ok(signature)) => Some(Asset {
            url: url.to_string(),
            signature: signature.clone(),
        }),
        _ => None,
    };
    Some(Latest {
        version: release.version,
        asset,
    })
}

/// Whether `candidate` is above `than` by SemVer precedence: pre-release
/// below its release, build metadata ignored (DST-R52).
pub fn is_above(candidate: &Version, than: &Version) -> bool {
    candidate.cmp_precedence(than) == Ordering::Greater
}

/// Why a download isn't trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The public key or the signature can't be read.
    Malformed(String),
    /// The signature isn't the key's over these bytes.
    BadSignature(String),
    /// Signed for another version than `latest.json` announced: an old
    /// Release offered as a new one (DST-R52).
    OtherVersion { signed: String },
    /// Signed as another platform's bundle: the other OS's file of the same
    /// Release, which would leave a Companion that can't start.
    OtherBundle { signed: String },
    /// The signed trusted comment names no file, so the file could be any
    /// Release's, for either platform.
    NoFile,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Malformed(e) => write!(f, "unreadable key or signature: {e}"),
            VerifyError::BadSignature(e) => write!(f, "bad signature: {e}"),
            VerifyError::OtherVersion { signed } => write!(f, "signed for version {signed}"),
            VerifyError::OtherBundle { signed } => write!(f, "signed as the file {signed:?}"),
            VerifyError::NoFile => write!(f, "the signature names no file"),
        }
    }
}

/// Verifies `data` against the base64 public key from `plugins.updater
/// .pubkey` and the base64 signature from `latest.json` (DST-R53), as the
/// plugin does: minisign, legacy signatures allowed. Then the signature must
/// be for `announced`, and for this platform's bundle, whose names end in
/// `bundle` ([`Keeper::bundle_suffix`](super::keep::Keeper::bundle_suffix)).
/// `latest.json` itself isn't signed: without this, an old Release's file,
/// validly signed, could be offered as a new version and installed over a
/// fixed one, which DST-R52 forbids; or the other OS's file of a Release
/// could replace the AppImage, which then can't start or update again.
///
/// Both come from the signed trusted comment, which is only trusted once the
/// signature has verified. `tauri signer` writes
/// `timestamp:<t>\tfile:<bundle name>`, and the bundle's name carries its
/// version ([`file_version`]) and ends like its platform's bundles. A
/// `version:` field, which other signers write, must be `announced` too.
pub fn verify(
    pubkey: &str,
    data: &[u8],
    signature: &str,
    announced: &Version,
    bundle: &str,
) -> Result<(), VerifyError> {
    let public_key = PublicKey::decode(&base64_text(pubkey)?)
        .map_err(|e| VerifyError::Malformed(e.to_string()))?;
    let signature = Signature::decode(&base64_text(signature)?)
        .map_err(|e| VerifyError::Malformed(e.to_string()))?;
    public_key
        .verify(data, &signature, true)
        .map_err(|e| VerifyError::BadSignature(e.to_string()))?;
    let is_announced = |signed: &str| Version::parse(signed).is_ok_and(|v| v == *announced);
    let mut file = None;
    for field in signature.trusted_comment().split('\t') {
        if let Some(version) = field.strip_prefix("version:") {
            let version = version.trim_start_matches('v');
            if !is_announced(version) {
                return Err(VerifyError::OtherVersion {
                    signed: version.to_owned(),
                });
            }
        } else if let Some(name) = field.strip_prefix("file:") {
            file = Some(name);
        }
    }
    let file = file.ok_or(VerifyError::NoFile)?;
    let version = file_version(file).unwrap_or(file);
    if !is_announced(version) {
        return Err(VerifyError::OtherVersion {
            signed: version.to_owned(),
        });
    }
    if !file.ends_with(bundle) {
        return Err(VerifyError::OtherBundle {
            signed: file.to_owned(),
        });
    }
    Ok(())
}

/// The version in a bundle's file name as Tauri names it,
/// `<product>_<version>_<arch and suffix>`: `Pagle Companion_1.4.2_x64-setup.exe`
/// or `Pagle Companion_1.4.2_amd64.AppImage` (DST Contracts, "Release
/// assets"). Neither the architecture nor the suffix holds a `_`.
pub fn file_version(file: &str) -> Option<&str> {
    let mut parts = file.rsplitn(3, '_');
    let _arch = parts.next()?;
    let version = parts.next()?;
    let _product = parts.next()?;
    Some(version)
}

fn base64_text(encoded: &str) -> Result<String, VerifyError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| VerifyError::Malformed(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| VerifyError::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(version: &str) -> Version {
        Version::parse(version).unwrap()
    }

    /// DST Contracts, "Release assets": the example `latest.json`.
    const EXAMPLE: &str = r#"{ "version": "1.4.2",
  "notes": "<the Release body>",
  "pub_date": "2026-11-20T10:00:00Z",
  "platforms": {
    "windows-x86_64": { "signature": "<.sig content>", "url": "https://github.com/timemir/pagle-companion/releases/download/v1.4.2/Pagle.Companion_1.4.2_x64-setup.exe" },
    "linux-x86_64":   { "signature": "<.sig content>", "url": "https://github.com/timemir/pagle-companion/releases/download/v1.4.2/Pagle.Companion_1.4.2_amd64.AppImage" } } }"#;

    #[test]
    fn the_endpoint_is_the_latest_releases_latest_json() {
        // DST-R50, DST Contracts "Tauri configuration": the single endpoint
        let config = plugin_config();
        assert_eq!(
            config
                .endpoints
                .iter()
                .map(|u| u.as_str())
                .collect::<Vec<_>>(),
            ["https://github.com/timemir/pagle-companion/releases/latest/download/latest.json"]
        );
        assert_eq!(
            resolve_endpoint(false, None, &config),
            "https://github.com/timemir/pagle-companion/releases/latest/download/latest.json"
        );
    }

    #[test]
    fn a_release_build_ignores_the_endpoint_variable() {
        // DST-R50: release builds use only the fixed URL
        let config = plugin_config();
        assert_eq!(
            resolve_endpoint(
                false,
                Some("http://127.0.0.1:9/latest.json".into()),
                &config
            ),
            "https://github.com/timemir/pagle-companion/releases/latest/download/latest.json"
        );
        assert_eq!(
            resolve_endpoint(
                true,
                Some(" http://127.0.0.1:9/latest.json ".into()),
                &config
            ),
            "http://127.0.0.1:9/latest.json"
        );
        assert_eq!(
            resolve_endpoint(true, Some(" ".into()), &config),
            "https://github.com/timemir/pagle-companion/releases/latest/download/latest.json"
        );
    }

    #[test]
    fn the_embedded_public_key_is_a_minisign_key() {
        // DST-R47, DST-R53: verified against the embedded public key
        let pubkey = plugin_config().pubkey;
        assert!(PublicKey::decode(&base64_text(&pubkey).unwrap()).is_ok());
    }

    #[test]
    fn the_platform_key_is_this_builds() {
        // DST Contracts, "Release assets": exactly windows-x86_64 and
        // linux-x86_64
        let expected = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        assert_eq!(platform(), expected);
    }

    #[test]
    fn latest_json_in_the_contracts_shape_is_read() {
        // DST Contracts, "Release assets": Tauri's static format, with the
        // two platform keys
        let windows = read_latest(EXAMPLE.as_bytes(), "windows-x86_64").unwrap();
        assert_eq!(windows.version, v("1.4.2"));
        assert_eq!(
            windows.asset,
            Some(Asset {
                url: "https://github.com/timemir/pagle-companion/releases/download/v1.4.2/Pagle.Companion_1.4.2_x64-setup.exe".into(),
                signature: "<.sig content>".into(),
            })
        );
        let linux = read_latest(EXAMPLE.as_bytes(), "linux-x86_64").unwrap();
        assert!(linux.asset.unwrap().url.ends_with("_amd64.AppImage"));
    }

    #[test]
    fn a_missing_platform_is_read_without_a_file() {
        // DST Contracts, "Release assets": only the two platform keys
        let latest = read_latest(EXAMPLE.as_bytes(), "darwin-aarch64").unwrap();
        assert_eq!(latest.version, v("1.4.2"));
        assert_eq!(latest.asset, None);
    }

    #[test]
    fn a_bad_document_is_not_read() {
        // DST-R51: a bad document is a failed check
        for body in [
            "",
            "<html>Not Found</html>",
            "{}",
            r#"{ "version": "one", "platforms": {} }"#,
            r#"{ "version": "1.4.2", "pub_date": "yesterday", "platforms": {} }"#,
        ] {
            assert_eq!(read_latest(body.as_bytes(), "linux-x86_64"), None, "{body}");
        }
    }

    #[test]
    fn only_a_higher_version_is_offered() {
        // DST-R52: higher by SemVer precedence; never an older or equal one
        assert!(is_above(&v("1.2.1"), &v("1.2.0")));
        assert!(is_above(&v("1.10.0"), &v("1.9.9")));
        assert!(!is_above(&v("1.2.0"), &v("1.2.0")));
        assert!(!is_above(&v("1.1.9"), &v("1.2.0")));
        // Build metadata doesn't count in precedence.
        assert!(!is_above(&v("1.2.0+build.2"), &v("1.2.0")));
    }

    const ENDPOINT: &str =
        "https://github.com/timemir/pagle-companion/releases/latest/download/latest.json";

    #[test]
    fn a_releases_file_is_downloaded() {
        // DST Contracts, "Release assets": latest.json names files of the
        // Release, which publish writes from the assets attached (DST-R46)
        assert!(is_release_file(
            ENDPOINT,
            "https://github.com/timemir/pagle-companion/releases/download/v1.4.2/Pagle.Companion_1.4.2_x64-setup.exe"
        ));
        // A test server's latest.json and files share its root.
        assert!(is_release_file(
            "http://127.0.0.1:9/latest.json",
            "http://127.0.0.1:9/download/v1.4.2/Pagle.Companion_1.4.2_x64-setup.exe"
        ));
    }

    #[test]
    fn a_file_outside_the_releases_is_never_downloaded() {
        // DST-R50: the single endpoint; latest.json can't point elsewhere
        for url in [
            "https://evil.example/Pagle.Companion_1.4.2_x64-setup.exe",
            "http://github.com/timemir/pagle-companion/releases/download/v1.4.2/a.exe",
            "https://github.com:8443/timemir/pagle-companion/releases/download/v1.4.2/a.exe",
            "https://github.com/someone/else/releases/download/v1.4.2/a.exe",
            "https://github.com/timemir/pagle-companion/releases/download/../../../../someone/else/releases/download/v1/a.exe",
            "https://github.com/timemir/pagle-companion/releases/download/%2e%2e/%2e%2e/%2e%2e/%2e%2e/someone/a.exe",
            "https://github.com/timemir/pagle-companion-evil/releases/download/v1.4.2/a.exe",
            "https://user:pass@github.com/timemir/pagle-companion/releases/download/v1.4.2/a.exe",
            "https://github.com/timemir/pagle-companion/releases/",
            "file:///C:/Windows/System32/calc.exe",
            "not a url",
        ] {
            assert!(!is_release_file(ENDPOINT, url), "{url}");
        }
    }

    #[test]
    fn a_bundles_file_name_carries_its_version() {
        // DST Contracts, "Release assets": <product>_<version>_<arch…>
        assert_eq!(
            file_version("Pagle Companion_1.4.2_x64-setup.exe"),
            Some("1.4.2")
        );
        assert_eq!(
            file_version("Pagle.Companion_1.4.2_amd64.AppImage"),
            Some("1.4.2")
        );
        assert_eq!(
            file_version("Pagle_Companion_0.0.1-test.7_amd64.AppImage"),
            Some("0.0.1-test.7")
        );
        assert_eq!(file_version("installer.exe"), None);
        assert_eq!(file_version("1.4.2_x64-setup.exe"), None);
    }

    #[test]
    fn a_tester_build_updates_to_the_release_above_it() {
        // DST-R48, DST-R52: 0.4.1 beats 0.4.1-test.7, and 0.0.1-test.5 updates
        // to a higher Release; the next tester build doesn't go backwards
        assert!(is_above(&v("0.4.1"), &v("0.4.1-test.7")));
        assert!(is_above(&v("0.0.1"), &v("0.0.1-test.5")));
        assert!(is_above(&v("0.1.0"), &v("0.0.1-test.5")));
        assert!(!is_above(&v("0.4.0"), &v("0.4.1-test.7")));
        assert!(is_above(&v("0.4.1-test.10"), &v("0.4.1-test.7")));
    }
}
