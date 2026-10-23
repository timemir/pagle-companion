//! The config read (DST-R63, DST Contracts "Companion config endpoint"):
//! `GET /api/v1/companion/config`, with the version header and no
//! credential. A reply that isn't `200` JSON with a valid `MAJOR.MINOR.PATCH`
//! `minCompanionVersion` and a non-blank `foreverProductUid` is a failed
//! read (#18). Beside them, `recommendedCompanionVersion` (pagle-backend#287)
//! is `0.0.0` when missing, so a backend from before it breaks nothing; one
//! that isn't `MAJOR.MINOR.PATCH` reads as `0.0.0` too, since only the
//! nudge hangs off it (`update_policy`).

use reqwest::Method;
use reqwest::header::{ACCEPT, HeaderValue};
use semver::Version;
use serde::Deserialize;

use crate::api::{self, Api};

/// The endpoint, under `/api/v1/`.
pub const PATH: &str = "companion/config";

/// What a successful config read says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub min_companion_version: Version,
    /// Below it, a waiting update nudges the Player (pagle-backend#287).
    pub recommended_companion_version: Version,
    /// Served as it is, and compared byte for byte (CMP-R12).
    pub forever_product_uid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    min_companion_version: String,
    #[serde(default)]
    recommended_companion_version: Option<serde_json::Value>,
    forever_product_uid: String,
}

/// One config read. The error says why it failed, for the log.
pub async fn read(api: &Api) -> Result<Config, String> {
    let reply = api
        .request(Method::GET, PATH)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if reply.status() != reqwest::StatusCode::OK {
        return Err(format!("status {}", reply.status()));
    }
    let body = api::read_capped(reply, api::REPLY_CAP)
        .await
        .map_err(|e| e.to_string())?;
    parse(&body).ok_or_else(|| "a bad config body".into())
}

/// A `200` body, if it is a valid config.
pub fn parse(body: &[u8]) -> Option<Config> {
    let body: Body = serde_json::from_slice(body).ok()?;
    let min = release_version(&body.min_companion_version)?;
    if body.forever_product_uid.trim().is_empty() {
        return None;
    }
    let recommended = match &body.recommended_companion_version {
        None => Version::new(0, 0, 0),
        Some(value) => value.as_str().and_then(release_version).unwrap_or_else(|| {
            log::warn!("recommendedCompanionVersion {value} isn't a version; taking it as 0.0.0");
            Version::new(0, 0, 0)
        }),
    };
    Some(Config {
        min_companion_version: min,
        recommended_companion_version: recommended,
        forever_product_uid: body.forever_product_uid,
    })
}

/// `MAJOR.MINOR.PATCH` with no suffix (DST-R64).
fn release_version(text: &str) -> Option<Version> {
    Version::parse(text)
        .ok()
        .filter(|v| v.pre.is_empty() && v.build.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub_server;
    use wiremock::ResponseTemplate;

    fn json(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "application/json")
    }

    #[tokio::test]
    async fn a_read_carries_the_version_header_and_no_credential() {
        // DST-R63: no authentication; DST-R60: the version header on every
        // /api/v1 request
        let server = stub_server::start().await;
        stub_server::script(
            &server,
            "GET",
            PATH,
            vec![json(
                r#"{ "minCompanionVersion": "1.2.0", "foreverProductUid": "wow_forever" }"#,
            )],
        )
        .await;

        let config = read(&stub_server::api(&server)).await.unwrap();
        assert_eq!(
            config,
            Config {
                min_companion_version: Version::new(1, 2, 0),
                recommended_companion_version: Version::new(0, 0, 0),
                forever_product_uid: "wow_forever".into(),
            }
        );
        let requests = stub_server::requests(&server, PATH).await;
        assert_eq!(requests.len(), 1);
        let headers = &requests[0].headers;
        assert_eq!(headers.get("pagle-companion-version").unwrap(), "1.0.0");
        assert!(headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn anything_but_a_200_is_a_failed_read() {
        // #18: a response that isn't 200 JSON is a failed read
        let server = stub_server::start().await;
        let good = r#"{ "minCompanionVersion": "1.2.0", "foreverProductUid": "x" }"#;
        stub_server::script(
            &server,
            "GET",
            PATH,
            vec![
                ResponseTemplate::new(500),
                ResponseTemplate::new(204),
                ResponseTemplate::new(403).set_body_raw(good.as_bytes(), "application/json"),
            ],
        )
        .await;
        let api = stub_server::api(&server);
        for _ in 0..3 {
            assert!(read(&api).await.is_err());
        }
    }

    #[test]
    fn a_body_without_a_valid_minimum_or_uid_is_a_failed_read() {
        // #18: a valid MAJOR.MINOR.PATCH minCompanionVersion (DST-R64) and a
        // non-blank foreverProductUid, or nothing changes
        for body in [
            "not json",
            r#"{ "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "1.2.0" }"#,
            r#"{ "minCompanionVersion": "1.2", "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "v1.2.0", "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "1.2.0-rc.1", "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "1.2.0+5", "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "1.2.0", "foreverProductUid": "  " }"#,
            r#"{ "minCompanionVersion": 1, "foreverProductUid": "x" }"#,
        ] {
            assert_eq!(parse(body.as_bytes()), None, "{body}");
        }
    }

    #[test]
    fn the_uid_is_kept_as_served() {
        // DST-R64: served as it is; CMP-R12: compared byte for byte
        let config =
            parse(br#"{ "minCompanionVersion": "0.0.0", "foreverProductUid": " wow_x " }"#)
                .unwrap();
        assert_eq!(config.forever_product_uid, " wow_x ");
    }

    #[test]
    fn the_recommended_version_is_read_beside_the_minimum() {
        // pagle-backend#287: the config also returns
        // recommendedCompanionVersion, MAJOR.MINOR.PATCH
        let config = parse(
            br#"{ "minCompanionVersion": "1.0.0", "recommendedCompanionVersion": "1.3.0",
                  "foreverProductUid": "wow_forever" }"#,
        )
        .unwrap();
        assert_eq!(config.min_companion_version, Version::new(1, 0, 0));
        assert_eq!(config.recommended_companion_version, Version::new(1, 3, 0));
    }

    #[test]
    fn a_missing_recommended_version_is_0_0_0() {
        // pagle-backend#287: a missing field means 0.0.0, so an older backend
        // doesn't break the Companion; a value that isn't MAJOR.MINOR.PATCH
        // loses only the nudge, never the minimum or the uid
        for body in [
            r#"{ "minCompanionVersion": "1.2.0", "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "1.2.0", "recommendedCompanionVersion": null, "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "1.2.0", "recommendedCompanionVersion": "1.3", "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "1.2.0", "recommendedCompanionVersion": "1.3.0-rc.1", "foreverProductUid": "x" }"#,
            r#"{ "minCompanionVersion": "1.2.0", "recommendedCompanionVersion": 13, "foreverProductUid": "x" }"#,
        ] {
            let config = parse(body.as_bytes()).unwrap_or_else(|| panic!("{body}"));
            assert_eq!(
                config.recommended_companion_version,
                Version::new(0, 0, 0),
                "{body}"
            );
            assert_eq!(
                config.min_companion_version,
                Version::new(1, 2, 0),
                "{body}"
            );
            assert_eq!(config.forever_product_uid, "x", "{body}");
        }
    }
}
