//! A stub of the backend for tests: each endpoint answers a script of
//! replies, in order, and the server records every request.

use std::collections::VecDeque;
use std::sync::Mutex;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::api::Api;

/// Replies in order; once the script runs out, every request gets a `500`.
struct Script(Mutex<VecDeque<ResponseTemplate>>);

impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ResponseTemplate::new(500))
    }
}

pub async fn start() -> MockServer {
    MockServer::start().await
}

/// An `Api` for the stub, as version `1.0.0`.
pub fn api(server: &MockServer) -> Api {
    Api::new("1.0.0", server.uri()).unwrap()
}

/// Answers `verb /api/v1/<endpoint>` with `replies`, in order.
pub async fn script(
    server: &MockServer,
    verb: &str,
    endpoint: &str,
    replies: Vec<ResponseTemplate>,
) {
    Mock::given(method(verb))
        .and(path(format!("/api/v1/{endpoint}")))
        .respond_with(Script(Mutex::new(replies.into())))
        .mount(server)
        .await;
}

/// The requests the stub received for `/api/v1/<endpoint>`.
pub async fn requests(server: &MockServer, endpoint: &str) -> Vec<Request> {
    let wanted = format!("/api/v1/{endpoint}");
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path() == wanted)
        .collect()
}

/// A `{ "error": code }` reply, the device flow's error shape.
pub fn device_error(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(serde_json::json!({ "error": code }))
}

/// A `{ "reason": code, "message": … }` reply, the credential endpoints'
/// error shape.
pub fn refusal(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_json(serde_json::json!({ "reason": code, "message": "refused" }))
}

/// A base URL nothing listens on.
pub fn unreachable_base() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    base
}
