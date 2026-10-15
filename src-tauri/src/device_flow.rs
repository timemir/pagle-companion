//! The device flow from the Companion's side (CMP-R60; ACC-R42, ACC-R44):
//! `POST /api/v1/companion/device` with the host name and platform, then
//! `POST /api/v1/companion/token` every interval plus the poll margin until
//! the Player approves, cancels, or the code expires. Neither request carries
//! a credential (CMP-R49).
//!
//! What happens with the credential once approved is `link`'s; the window's
//! part is `linking`'s.

use std::future::Future;
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::Deserialize;

use crate::api::Api;

/// Device name length (CMP Tunables, ACC Tunables), in code points.
pub const DEVICE_NAME_LENGTH: usize = 64;

/// Poll margin (CMP Tunables): added to the interval between polls.
pub const POLL_MARGIN: Duration = Duration::from_secs(1);

/// Slow-down step (CMP Tunables, ACC Tunables): added to the interval on
/// `slow_down`.
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// The device name when the host name leaves nothing (CMP-R60).
pub const FALLBACK_DEVICE_NAME: &str = "Pagle Companion";

/// `platform` in the device request (ACC Contracts).
pub const PLATFORM: &str = if cfg!(windows) { "windows" } else { "linux" };

/// The device-code grant type (RFC 8628, ACC Contracts).
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The `update_required` message (DST Contracts), when the refusal carries no
/// `error_description`.
pub const UPDATE_REQUIRED_TEXT: &str =
    "This version of Pagle Companion is too old. Update it to keep uploading.";

/// The device name sent for a host name (CMP-R60): trimmed, with Unicode
/// control characters (category Cc) removed, cut to the device name length,
/// trimmed again, or "Pagle Companion" when that leaves nothing.
pub fn device_name(host: &str) -> String {
    let kept: String = host.trim().chars().filter(|c| !c.is_control()).collect();
    let cut: String = kept.chars().take(DEVICE_NAME_LENGTH).collect();
    match cut.trim() {
        "" => FALLBACK_DEVICE_NAME.to_owned(),
        name => name.to_owned(),
    }
}

/// This computer's host name: `GetComputerNameExW` with
/// `ComputerNameDnsHostname` on Windows, `gethostname` on Linux. Empty when
/// it can't be read.
pub fn host_name() -> String {
    read_host_name().unwrap_or_default()
}

#[cfg(windows)]
fn read_host_name() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNameDnsHostname, GetComputerNameExW,
    };
    let mut size: u32 = 0;
    // SAFETY: a null buffer asks for the size, written to `size`.
    unsafe { GetComputerNameExW(ComputerNameDnsHostname, std::ptr::null_mut(), &mut size) };
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: `buffer` holds `size` UTF-16 units.
    let read =
        unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut size) };
    // On success `size` excludes the terminating NUL.
    (read != 0).then(|| String::from_utf16_lossy(&buffer[..size as usize]))
}

#[cfg(target_os = "linux")]
fn read_host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: `buffer` is writable for its whole length.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    let len = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..len]).into_owned())
}

/// The device request's answer (ACC Contracts). Its `Debug` shows the user
/// code only.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceCode[{}]", self.user_code)
    }
}

/// An approved link: the credential and the Player's BattleTag (`""` when
/// none is stored, ACC-R44). Its `Debug` shows neither (CMP-R62, CMP-R112).
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Approval {
    pub credential: String,
    pub battle_tag: String,
}

impl std::fmt::Debug for Approval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Approval[hidden]")
    }
}

/// How linking failed, each with CMP-R60's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A connection error, timeout or `5xx` (on a poll: until the code's
    /// `expires_in` has passed).
    Unreachable,
    /// A `429` on the device request.
    TooManyAttempts,
    /// `expired_token`.
    Expired,
    /// `access_denied`: the Player pressed Cancel on the approval page.
    Denied,
    /// Any other refusal.
    Failed,
    /// `update_required` (DST-R61), with its message: linking stops and the
    /// Companion enters update required (CMP-R50).
    UpdateRequired(String),
}

impl Failure {
    /// What the window shows (CMP-R60).
    pub fn text(&self) -> String {
        match self {
            Failure::Unreachable => "Pagle can't be reached. Try again in a few minutes.".into(),
            Failure::TooManyAttempts => {
                "Too many link attempts from this network. Try again later.".into()
            }
            Failure::Expired => "The code expired. Start linking again.".into(),
            Failure::Denied => "Linking was cancelled.".into(),
            Failure::Failed => "Linking failed. Start linking again.".into(),
            Failure::UpdateRequired(message) => message.clone(),
        }
    }
}

/// The device flow's error body, `{ "error": … }`, with `error_description`
/// on `update_required` only (ACC Contracts, DST Contracts).
#[derive(Debug, Default, Deserialize)]
struct DeviceError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

impl DeviceError {
    async fn read(reply: reqwest::Response) -> Self {
        reply.json().await.unwrap_or_default()
    }

    fn update_required(&self) -> Option<Failure> {
        (self.error == "update_required").then(|| {
            Failure::UpdateRequired(
                self.error_description
                    .clone()
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| UPDATE_REQUIRED_TEXT.to_owned()),
            )
        })
    }
}

/// What one poll found.
enum Poll {
    Pending,
    SlowDown,
    Approved(Approval),
    /// A connection error, timeout or `5xx`.
    Unreachable,
    Ended(Failure),
}

/// How polling ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    Approved(Approval),
    Failed(Failure),
    /// The Player pressed Cancel in the window.
    Cancelled,
}

/// Starts a link: `POST /api/v1/companion/device` with `device_name` and
/// this platform (CMP-R60).
pub async fn start(api: &Api, device_name: &str) -> Result<DeviceCode, Failure> {
    let sent = api
        .request(Method::POST, "companion/device")
        .json(&serde_json::json!({ "device_name": device_name, "platform": PLATFORM }))
        .send()
        .await;
    let reply = match sent {
        Ok(reply) => reply,
        Err(e) => {
            log::warn!("the device request failed: {e}");
            return Err(Failure::Unreachable);
        }
    };
    let status = reply.status();
    if status.is_server_error() {
        log::warn!("the device request was answered {status}");
        return Err(Failure::Unreachable);
    }
    if status == StatusCode::OK {
        return reply.json().await.map_err(|e| {
            log::warn!("the device request's answer can't be read: {e}");
            Failure::Failed
        });
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        log::info!("the device request was rate limited");
        return Err(Failure::TooManyAttempts);
    }
    let error = DeviceError::read(reply).await;
    log::warn!("the device request was refused: {status} {:?}", error.error);
    Err(error.update_required().unwrap_or(Failure::Failed))
}

async fn poll_once(api: &Api, device_code: &str) -> Poll {
    let sent = api
        .request(Method::POST, "companion/token")
        .json(&serde_json::json!({ "grant_type": DEVICE_GRANT, "device_code": device_code }))
        .send()
        .await;
    let reply = match sent {
        Ok(reply) => reply,
        Err(e) => {
            log::warn!("a token poll failed: {e}");
            return Poll::Unreachable;
        }
    };
    let status = reply.status();
    if status.is_server_error() {
        log::warn!("a token poll was answered {status}");
        return Poll::Unreachable;
    }
    if status == StatusCode::OK {
        return match reply.json().await {
            Ok(approval) => Poll::Approved(approval),
            Err(e) => {
                log::warn!("the token answer can't be read: {e}");
                Poll::Ended(Failure::Failed)
            }
        };
    }
    let error = DeviceError::read(reply).await;
    if let Some(failure) = error.update_required() {
        return Poll::Ended(failure);
    }
    match error.error.as_str() {
        "authorization_pending" => Poll::Pending,
        "slow_down" => Poll::SlowDown,
        "expired_token" => Poll::Ended(Failure::Expired),
        "access_denied" => Poll::Ended(Failure::Denied),
        other => {
            log::warn!("a token poll was refused: {status} {other:?}");
            Poll::Ended(Failure::Failed)
        }
    }
}

/// Polls the token endpoint until the link is decided (CMP-R60). Each poll
/// comes one wait after the last, the first one wait after the code arrived;
/// `wait` sleeps that long and answers `false` once the Player cancels.
pub async fn poll<W, F>(api: &Api, code: &DeviceCode, mut wait: W) -> End
where
    W: FnMut(Duration) -> F,
    F: Future<Output = bool>,
{
    let mut interval = Duration::from_secs(code.interval);
    let lifetime = Duration::from_secs(code.expires_in);
    // Time is counted in waits, so the schedule doesn't depend on how long
    // each poll took.
    let mut waited = Duration::ZERO;
    loop {
        let pause = interval + POLL_MARGIN;
        if !wait(pause).await {
            return End::Cancelled;
        }
        waited += pause;
        match poll_once(api, &code.device_code).await {
            Poll::Pending => {}
            Poll::SlowDown => interval += SLOW_DOWN_STEP,
            Poll::Approved(approval) => return End::Approved(approval),
            Poll::Unreachable if waited >= lifetime => {
                return End::Failed(Failure::Unreachable);
            }
            Poll::Unreachable => {}
            Poll::Ended(failure) => return End::Failed(failure),
        }
    }
}

/// A QR code of `text` as an SVG element, dark on light, for the window
/// (CMP UI, Linking).
pub fn qr_svg(text: &str) -> Option<String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let image = code
        .render::<svg::Color>()
        .min_dimensions(200, 200)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    // The element only, without the XML declaration.
    image.find("<svg").map(|at| image[at..].to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wiremock::ResponseTemplate;

    use super::*;
    use crate::stub_server::{self, device_error};

    fn code(expires_in: u64, interval: u64) -> DeviceCode {
        DeviceCode {
            device_code: "dc_secret".into(),
            user_code: "BCDF-GHJK".into(),
            verification_uri: "https://pagle.app/link".into(),
            verification_uri_complete: "https://pagle.app/link?code=BCDF-GHJK".into(),
            expires_in,
            interval,
        }
    }

    fn approved(credential: &str, battle_tag: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(
            serde_json::json!({ "credential": credential, "battle_tag": battle_tag }),
        )
    }

    /// A wait that sleeps nothing and records each wait.
    fn recorder() -> (
        Arc<Mutex<Vec<Duration>>>,
        impl FnMut(Duration) -> std::future::Ready<bool>,
    ) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seen = waits.clone();
        (waits, move |d| {
            seen.lock().unwrap().push(d);
            std::future::ready(true)
        })
    }

    fn secs(waits: &Arc<Mutex<Vec<Duration>>>) -> Vec<u64> {
        waits.lock().unwrap().iter().map(|d| d.as_secs()).collect()
    }

    #[test]
    fn the_device_name_is_the_trimmed_host_name() {
        // CMP-R60: the host name, trimmed
        assert_eq!(device_name("  ADA-PC \n"), "ADA-PC");
    }

    #[test]
    fn the_device_name_drops_control_characters() {
        // CMP-R60: Unicode control characters (category Cc) removed, then
        // trimmed again
        assert_eq!(device_name("\u{7}ADA\u{0}-PC\u{1b}"), "ADA-PC");
        assert_eq!(device_name("\u{7} ADA-PC"), "ADA-PC");
    }

    #[test]
    fn the_device_name_is_cut_to_64_code_points() {
        // CMP-R60: cut to the device name length (64 code points), trimmed
        // again
        let long = "é".repeat(70);
        assert_eq!(device_name(&long).chars().count(), 64);
        let spaced = format!("{} {}", "a".repeat(63), "b".repeat(10));
        assert_eq!(device_name(&spaced), "a".repeat(63));
        assert_eq!(DEVICE_NAME_LENGTH, 64);
    }

    #[test]
    fn an_empty_host_name_is_pagle_companion() {
        // CMP-R60: "Pagle Companion" when that leaves nothing
        assert_eq!(device_name(""), "Pagle Companion");
        assert_eq!(device_name(" \u{7}\t"), "Pagle Companion");
    }

    #[test]
    fn this_computer_has_a_host_name() {
        // CMP-R60: GetComputerNameExW / gethostname
        assert!(!host_name().trim().is_empty());
    }

    #[tokio::test]
    async fn the_device_request_sends_the_name_and_platform_and_no_credential() {
        // CMP-R60: device_name and platform; CMP-R49: the device-flow
        // requests carry no Authorization
        let server = stub_server::start().await;
        stub_server::script(
            &server,
            "POST",
            "companion/device",
            vec![ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "dc_secret",
                "user_code": "BCDF-GHJK",
                "verification_uri": "https://pagle.app/link",
                "verification_uri_complete": "https://pagle.app/link?code=BCDF-GHJK",
                "expires_in": 900,
                "interval": 5
            }))],
        )
        .await;

        let started = start(&stub_server::api(&server), "ADA-PC").await.unwrap();
        assert_eq!(started, code(900, 5));

        let sent = stub_server::requests(&server, "companion/device").await;
        assert_eq!(sent.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "device_name": "ADA-PC", "platform": PLATFORM })
        );
        assert!(sent[0].headers.get("authorization").is_none());
        assert_eq!(
            sent[0].headers.get("content-type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn the_platform_is_windows_or_linux() {
        // CMP-R60: platform = windows or linux
        assert_eq!(PLATFORM, if cfg!(windows) { "windows" } else { "linux" });
    }

    async fn start_with(reply: ResponseTemplate) -> Result<DeviceCode, Failure> {
        let server = stub_server::start().await;
        stub_server::script(&server, "POST", "companion/device", vec![reply]).await;
        start(&stub_server::api(&server), "ADA-PC").await
    }

    #[tokio::test]
    async fn a_429_start_says_too_many_link_attempts() {
        // CMP-R60: 429 shows "Too many link attempts from this network. Try
        // again later."
        let failure = start_with(device_error(429, "rate_limited"))
            .await
            .unwrap_err();
        assert_eq!(failure, Failure::TooManyAttempts);
        assert_eq!(
            failure.text(),
            "Too many link attempts from this network. Try again later."
        );
    }

    #[tokio::test]
    async fn a_5xx_start_says_pagle_cant_be_reached() {
        // CMP-R60: a 5xx shows "Pagle can't be reached. Try again in a few
        // minutes."
        let failure = start_with(ResponseTemplate::new(503)).await.unwrap_err();
        assert_eq!(failure, Failure::Unreachable);
        assert_eq!(
            failure.text(),
            "Pagle can't be reached. Try again in a few minutes."
        );
    }

    #[tokio::test]
    async fn an_unreachable_server_says_pagle_cant_be_reached() {
        // CMP-R60: a connection error shows "Pagle can't be reached."
        let api = Api::new("1.0.0", stub_server::unreachable_base()).unwrap();
        assert_eq!(
            start(&api, "ADA-PC").await.unwrap_err(),
            Failure::Unreachable
        );
    }

    #[tokio::test]
    async fn update_required_on_start_stops_linking() {
        // CMP-R60: update_required enters update required (CMP-R50)
        let reply = ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "update_required",
            "error_description": "Update Pagle Companion."
        }));
        assert_eq!(
            start_with(reply).await.unwrap_err(),
            Failure::UpdateRequired("Update Pagle Companion.".into())
        );
    }

    #[tokio::test]
    async fn any_other_start_refusal_says_linking_failed() {
        // CMP-R60: anything else shows "Linking failed. Start linking again."
        let failure = start_with(device_error(400, "invalid_request"))
            .await
            .unwrap_err();
        assert_eq!(failure, Failure::Failed);
        assert_eq!(failure.text(), "Linking failed. Start linking again.");
        let garbled = ResponseTemplate::new(200).set_body_string("not json");
        assert_eq!(start_with(garbled).await.unwrap_err(), Failure::Failed);
    }

    #[tokio::test]
    async fn authorization_pending_keeps_polling_until_approved() {
        // CMP-R60: polls every interval plus the poll margin, the first one
        // wait after the code arrives; authorization_pending keeps polling
        let server = stub_server::start().await;
        stub_server::script(
            &server,
            "POST",
            "companion/token",
            vec![
                device_error(400, "authorization_pending"),
                device_error(400, "authorization_pending"),
                approved("pgc_new", "Ada#1234"),
            ],
        )
        .await;
        let (waits, wait) = recorder();

        let end = poll(&stub_server::api(&server), &code(900, 5), wait).await;
        assert_eq!(
            end,
            End::Approved(Approval {
                credential: "pgc_new".into(),
                battle_tag: "Ada#1234".into()
            })
        );
        assert_eq!(secs(&waits), [6, 6, 6]);

        let polls = stub_server::requests(&server, "companion/token").await;
        assert_eq!(polls.len(), 3);
        let body: serde_json::Value = serde_json::from_slice(&polls[0].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "grant_type": DEVICE_GRANT, "device_code": "dc_secret" })
        );
        assert!(polls[0].headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn slow_down_adds_five_seconds() {
        // CMP-R60: slow_down adds the slow-down step (5 s) to the interval
        let server = stub_server::start().await;
        stub_server::script(
            &server,
            "POST",
            "companion/token",
            vec![
                device_error(400, "slow_down"),
                device_error(400, "authorization_pending"),
                device_error(400, "slow_down"),
                approved("pgc_new", ""),
            ],
        )
        .await;
        let (waits, wait) = recorder();

        poll(&stub_server::api(&server), &code(900, 5), wait).await;
        assert_eq!(secs(&waits), [6, 11, 11, 16]);
        assert_eq!(SLOW_DOWN_STEP, Duration::from_secs(5));
        assert_eq!(POLL_MARGIN, Duration::from_secs(1));
    }

    async fn poll_until(reply: ResponseTemplate) -> End {
        let server = stub_server::start().await;
        stub_server::script(&server, "POST", "companion/token", vec![reply]).await;
        let (_, wait) = recorder();
        poll(&stub_server::api(&server), &code(900, 5), wait).await
    }

    #[tokio::test]
    async fn expired_token_says_the_code_expired() {
        // CMP-R60: expired_token shows "The code expired. Start linking
        // again."
        let end = poll_until(device_error(400, "expired_token")).await;
        assert_eq!(end, End::Failed(Failure::Expired));
        assert_eq!(
            Failure::Expired.text(),
            "The code expired. Start linking again."
        );
    }

    #[tokio::test]
    async fn access_denied_says_linking_was_cancelled() {
        // CMP-R60: access_denied shows "Linking was cancelled."
        let end = poll_until(device_error(400, "access_denied")).await;
        assert_eq!(end, End::Failed(Failure::Denied));
        assert_eq!(Failure::Denied.text(), "Linking was cancelled.");
    }

    #[tokio::test]
    async fn other_poll_refusals_say_linking_failed() {
        // CMP-R60: invalid_request, unsupported_grant_type, invalid_grant
        // show "Linking failed. Start linking again."
        for code in ["invalid_request", "unsupported_grant_type", "invalid_grant"] {
            let end = poll_until(device_error(400, code)).await;
            assert_eq!(end, End::Failed(Failure::Failed), "{code}");
        }
    }

    #[tokio::test]
    async fn update_required_on_a_poll_stops_linking() {
        // CMP-R60: update_required stops linking
        let reply = ResponseTemplate::new(403)
            .set_body_json(serde_json::json!({ "error": "update_required" }));
        assert_eq!(
            poll_until(reply).await,
            End::Failed(Failure::UpdateRequired(UPDATE_REQUIRED_TEXT.into()))
        );
    }

    #[tokio::test]
    async fn an_unreachable_server_is_polled_until_the_code_expires() {
        // CMP-R60: a connection error, timeout or 5xx keeps polling at the
        // interval until expires_in has passed, then "Pagle can't be
        // reached."
        let server = stub_server::start().await;
        stub_server::script(
            &server,
            "POST",
            "companion/token",
            vec![
                ResponseTemplate::new(502),
                device_error(400, "authorization_pending"),
                ResponseTemplate::new(500),
                ResponseTemplate::new(503),
            ],
        )
        .await;
        let (waits, wait) = recorder();

        let end = poll(&stub_server::api(&server), &code(20, 4), wait).await;
        assert_eq!(end, End::Failed(Failure::Unreachable));
        // Four polls, 5 s apart: the fourth comes after 20 s, as the code
        // expires.
        assert_eq!(secs(&waits), [5, 5, 5, 5]);

        let api = Api::new("1.0.0", stub_server::unreachable_base()).unwrap();
        let (waits, wait) = recorder();
        let end = poll(&api, &code(12, 5), wait).await;
        assert_eq!(end, End::Failed(Failure::Unreachable));
        assert_eq!(secs(&waits), [6, 6]);
    }

    #[tokio::test]
    async fn cancel_stops_polling() {
        // CMP-R60: the Player can cancel, which stops polling
        let server = stub_server::start().await;
        stub_server::script(
            &server,
            "POST",
            "companion/token",
            vec![device_error(400, "authorization_pending")],
        )
        .await;
        let mut waits = 0;
        let end = poll(&stub_server::api(&server), &code(900, 5), |_| {
            waits += 1;
            std::future::ready(waits < 2)
        })
        .await;
        assert_eq!(end, End::Cancelled);
        assert_eq!(
            stub_server::requests(&server, "companion/token")
                .await
                .len(),
            1
        );
    }

    #[test]
    fn an_approval_never_shows_its_credential_or_battle_tag() {
        // CMP-R62, CMP-R112: the credential and BattleTag never reach the
        // local log file
        let approval = Approval {
            credential: "pgc_secret".into(),
            battle_tag: "Ada#1234".into(),
        };
        let shown = format!("{approval:?} {:?}", End::Approved(approval.clone()));
        assert!(!shown.contains("pgc_secret"), "{shown}");
        assert!(!shown.contains("Ada"), "{shown}");
    }

    #[test]
    fn the_qr_code_is_an_svg_of_the_complete_url() {
        // CMP-R60: a QR code of verification_uri_complete
        let svg = qr_svg("https://pagle.app/link?code=BCDF-GHJK").unwrap();
        assert!(svg.starts_with("<svg"), "{svg}");
        assert!(svg.ends_with("</svg>"), "{svg}");
    }
}
