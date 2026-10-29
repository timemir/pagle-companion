//! The one HTTP client every Companion request goes through (CMP Contracts,
//! HTTP; CMP-R6; DST-R60).

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// The base URL (OPS-R21).
pub const BASE_URL: &str = "https://pagle.app";

/// The variable a debug build takes another base URL from.
pub const BASE_URL_VAR: &str = "PAGLE_COMPANION_BASE_URL";

/// HTTP timeouts (CMP Tunables).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The Recovery request timeout (CMP Tunables): the whole Recovery planning
/// request, in place of [`REQUEST_TIMEOUT`]. It sends a snapshot of up to
/// 24 MiB, which the backend reads for up to its Recovery read deadline of
/// 2 min (UPL Tunables), then waits for the backend to replay the history and
/// reads a reply of up to 33 MiB; 120 s in all would time out first on a
/// slow link. Confirmation is small and keeps the default.
pub const RECOVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Carries the running version on every request (DST Contracts, Version header).
pub const VERSION_HEADER: HeaderName = HeaderName::from_static("pagle-companion-version");

/// The most bytes of a JSON reply the Companion reads: a cursor, an Upload
/// reply, a refusal, the device flow's answers or the config. Each is a few
/// hundred bytes; anything longer is a broken or hostile server, and is
/// refused before it can fill memory.
pub const REPLY_CAP: usize = 1024 * 1024;

/// The base URL this build talks to: `PAGLE_COMPANION_BASE_URL` in a debug
/// build when it is set, otherwise `https://pagle.app`. Release builds ignore
/// the variable.
pub fn base_url() -> String {
    resolve_base_url(cfg!(debug_assertions), std::env::var(BASE_URL_VAR).ok())
}

fn resolve_base_url(debug_build: bool, from_env: Option<String>) -> String {
    match from_env {
        Some(url) if debug_build && !url.trim().is_empty() => {
            url.trim().trim_end_matches('/').to_owned()
        }
        _ => BASE_URL.to_owned(),
    }
}

#[derive(Debug, Clone)]
pub struct Api {
    http: reqwest::Client,
    base_url: String,
}

impl Api {
    /// A client for `base_url` that sends `Pagle-Companion-Version: <version>`
    /// and nothing else about the machine: no `User-Agent`, no cookies
    /// (CMP-R6).
    ///
    /// It follows no redirect: no endpoint redirects, so a `3xx` is answered
    /// like any other refusal, and the credential and Catch logs never go
    /// anywhere but the base URL. A release build speaks HTTPS only (CMP
    /// Contracts, HTTP).
    pub fn new(version: &str, base_url: impl Into<String>) -> Result<Self, ApiError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            VERSION_HEADER,
            HeaderValue::from_str(version).map_err(|_| ApiError::BadVersion(version.to_owned()))?,
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(!cfg!(debug_assertions))
            .build()
            .map_err(ApiError::Client)?;
        Ok(Self {
            http,
            base_url: base_url.into(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Starts a request to `/api/v1/<path>`.
    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/api/v1/{}", self.base_url, path.trim_start_matches('/'));
        self.http.request(method, url)
    }
}

/// Why a reply's body wasn't read.
#[derive(Debug)]
pub enum BodyError {
    /// The body, decoded, is longer than the cap the reader was given.
    TooLarge { cap: usize },
    /// The connection failed mid-body, or the body doesn't decode.
    Http(reqwest::Error),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::TooLarge { cap } => write!(f, "the reply's body is over {cap} bytes"),
            BodyError::Http(e) => write!(f, "the reply's body didn't read: {e}"),
        }
    }
}

impl std::error::Error for BodyError {}

/// Reads `reply`'s body, decoded, refusing one longer than `cap` bytes.
/// A gzip body is decoded as it arrives, so a small body that inflates past
/// the cap stops at most one chunk past it, and a hostile or broken server
/// can't make the Companion hold more than that.
pub async fn read_capped(mut reply: reqwest::Response, cap: usize) -> Result<Vec<u8>, BodyError> {
    if reply.content_length().is_some_and(|len| len > cap as u64) {
        return Err(BodyError::TooLarge { cap });
    }
    let mut body = Vec::new();
    while let Some(chunk) = reply.chunk().await.map_err(BodyError::Http)? {
        if body.len() + chunk.len() > cap {
            return Err(BodyError::TooLarge { cap });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Debug)]
pub enum ApiError {
    BadVersion(String),
    Client(reqwest::Error),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::BadVersion(v) => write!(f, "version {v:?} can't be sent as a header"),
            ApiError::Client(e) => write!(f, "couldn't build the HTTP client: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Answers one request with `204` and hands back its head, lowercased.
    async fn capture_one_request() -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                head.extend_from_slice(&buf[..n]);
            }
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(head).unwrap().to_lowercase()
        });
        (base, handle)
    }

    #[tokio::test]
    async fn every_request_carries_the_version_header() {
        // DST-R60: every Companion API request carries Pagle-Companion-Version
        let (base, server) = capture_one_request().await;
        let api = Api::new("1.4.2-test.7", base).unwrap();

        let reply = api
            .request(reqwest::Method::GET, "catch-logs/x/cursor")
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 204);

        let head = server.await.unwrap();
        assert!(
            head.starts_with("get /api/v1/catch-logs/x/cursor http/1.1\r\n"),
            "{head}"
        );
        assert!(
            head.contains("\r\npagle-companion-version: 1.4.2-test.7\r\n"),
            "{head}"
        );
    }

    #[tokio::test]
    async fn requests_send_nothing_else_about_the_machine() {
        // CMP-R6: nothing about the machine but what the endpoints carry
        let (base, server) = capture_one_request().await;
        let api = Api::new("1.0.0", base).unwrap();

        api.request(reqwest::Method::GET, "/companion/config")
            .send()
            .await
            .unwrap();

        let head = server.await.unwrap();
        assert!(!head.contains("user-agent"), "{head}");
        assert!(!head.contains("cookie"), "{head}");
    }

    #[test]
    fn a_release_build_ignores_the_base_url_variable() {
        // CMP Contracts, HTTP: release builds ignore PAGLE_COMPANION_BASE_URL
        assert_eq!(
            resolve_base_url(false, Some("http://localhost:8080".into())),
            "https://pagle.app"
        );
    }

    #[test]
    fn a_debug_build_takes_the_base_url_variable() {
        // CMP Contracts, HTTP: a debug build takes another base URL from
        // PAGLE_COMPANION_BASE_URL
        assert_eq!(
            resolve_base_url(true, Some("http://localhost:8080/".into())),
            "http://localhost:8080"
        );
        assert_eq!(resolve_base_url(true, None), "https://pagle.app");
        assert_eq!(
            resolve_base_url(true, Some("  ".into())),
            "https://pagle.app"
        );
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        // The credential and Catch logs go to the base URL only: a 3xx comes
        // back as the reply
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", "https://elsewhere.example/"),
            )
            .mount(&server)
            .await;
        let api = Api::new("1.0.0", server.uri()).unwrap();

        let reply = api
            .request(reqwest::Method::POST, "uploads")
            .bearer_auth("pgc_secret")
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 307);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// A reply of `template`, with the server that sent it, which must live
    /// while the body is read.
    async fn reply_with(
        template: wiremock::ResponseTemplate,
    ) -> (wiremock::MockServer, reqwest::Response) {
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer};
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(template)
            .mount(&server)
            .await;
        let api = Api::new("1.0.0", server.uri()).unwrap();
        let reply = api.request(reqwest::Method::GET, "x").send().await.unwrap();
        (server, reply)
    }

    #[tokio::test]
    async fn a_body_up_to_the_cap_is_read() {
        let body = vec![b'x'; 1000];
        let (_server, reply) =
            reply_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body.clone())).await;
        assert_eq!(read_capped(reply, 1000).await.unwrap(), body);
    }

    #[tokio::test]
    async fn a_body_past_the_cap_is_refused() {
        let (_server, reply) =
            reply_with(wiremock::ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 1001])).await;
        assert!(matches!(
            read_capped(reply, 1000).await,
            Err(BodyError::TooLarge { cap: 1000 })
        ));
    }

    #[tokio::test]
    async fn a_gzip_body_that_inflates_past_the_cap_is_refused() {
        // A few kilobytes of gzip that decode to 64 MiB stop at the cap
        use std::io::Write as _;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gzip.write_all(&vec![0u8; 64 * 1024 * 1024]).unwrap();
        let bomb = gzip.finish().unwrap();
        assert!(bomb.len() < REPLY_CAP / 4);
        let (_server, reply) = reply_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-encoding", "gzip")
                .set_body_bytes(bomb),
        )
        .await;
        assert!(matches!(
            read_capped(reply, REPLY_CAP).await,
            Err(BodyError::TooLarge { .. })
        ));
    }

    #[test]
    fn timeouts_are_ten_seconds_to_connect_and_two_minutes_per_request() {
        // CMP Tunables: HTTP timeouts
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(120));
    }

    #[test]
    fn recovery_planning_has_five_minutes_per_request() {
        // CMP Tunables: Recovery request timeout, longer than the HTTP
        // timeouts' per-request one
        assert_eq!(RECOVERY_REQUEST_TIMEOUT, Duration::from_secs(300));
        assert!(RECOVERY_REQUEST_TIMEOUT > REQUEST_TIMEOUT);
    }
}
