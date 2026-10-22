//! A stub backend that keeps a cursor: it holds one Catch log per `logId`,
//! answers the cursor endpoint from it (UPL-R26), and applies Uploads as
//! UPL-R30 to R36 and UPL-R64 say (a tombstone, gap, divergence, the result,
//! unchanged, stale), with the Reply shape of UPL Contracts. Tests can make
//! it answer something else for the next requests of an endpoint (any
//! refusal of UPL Refusals with [`refusal`]), set the `catchLog` its
//! successes carry, and read every page it received.
//!
//! [`FakeInGame`] stubs the three in-game downloads the same way.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::GzEncoder;
use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::in_game::backend_bodies::{ANGLERS_EXAMPLE, FOLLOWING_EXAMPLE};
use crate::in_game::content_vector;
use crate::in_game::folders::{DataFile, etag_of};
use crate::upload::download::endpoint;

/// One page's upload document header, as the stub received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub log_id: String,
    pub as_of: u64,
    pub total: u64,
    pub from: u64,
    pub anchor: String,
    pub count: u64,
}

/// A held entry: its ID (UPL-R13) and bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    id: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Held {
    characters: Vec<String>,
    entries: Vec<Entry>,
    newest_as_of: u64,
}

/// The `catchLog` a success carries unless a test sets another: Pending
/// `forever-api`, the v1 state for everyone (ACC-R21).
fn forever_api() -> serde_json::Value {
    json!({ "verification": "pending", "pendingReason": "forever-api",
            "heldCharacters": 0, "leftOff": null })
}

/// A refusal of UPL Refusals (or DST's `update_required`, OPS-R67's
/// `rate_limited`), with its message, in the Reply shape of UPL Contracts.
pub fn refusal(status: u16, code: &str) -> ResponseTemplate {
    let message = match code {
        "suspended" => "This Player is suspended. Contact support@pagle.app.",
        "too_large" => "This upload is too large.",
        "malformed" => "This isn't a complete Pagle Catch log. Copy it again.",
        "unsupported_document" | "unsupported_format" => "Update Pagle.",
        "checksum" => "The text changed or was cut off while copying. Copy it again.",
        "dev_log" => "This Catch log is from a Pagle development version and can't be uploaded.",
        "log_cap" => "You already have 20 Catch logs on Pagle. Delete one to add another.",
        "entry_cap" => "This Catch log is longer than Pagle accepts.",
        "gap" => "Pagle is missing earlier entries of this Catch log.",
        "divergence" => "This Catch log differs from the copy on Pagle.",
        "stale" => "Pagle already has a newer copy of this Catch log. Nothing changed.",
        "deleted" => {
            "You deleted this Catch log on the Pagle website. Allow it again there to upload it."
        }
        "update_required" => {
            "This version of Pagle Companion is too old. Update it to keep uploading."
        }
        "rate_limited" => "Too many uploads. Try again later.",
        _ => "Refused.",
    };
    ResponseTemplate::new(status).set_body_json(json!({
        "result": "refused", "reason": code, "message": message, "logId": null,
        "end": null, "newestAsOf": null, "catchLog": null }))
}

#[derive(Default)]
struct Inner {
    logs: HashMap<String, Held>,
    /// `logId`s deleted on the website (UPL-R64).
    tombstones: HashSet<String>,
    /// What successes carry as `catchLog`; `None` for [`forever_api`].
    catch_log: Option<serde_json::Value>,
    /// What the next requests to each endpoint get instead; `None` lets one
    /// through to the stub's own answer.
    cursor_script: VecDeque<Option<ResponseTemplate>>,
    upload_script: VecDeque<Option<ResponseTemplate>>,
    cursor_reads: usize,
    pages: Vec<Seen>,
}

#[derive(Clone, Default)]
pub struct FakePagle(Arc<Mutex<Inner>>);

impl FakePagle {
    /// Mounts the stub's two endpoints on `server`.
    pub async fn mount(&self, server: &MockServer) {
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/v1/catch-logs/[0-9a-f]{16}/cursor$"))
            .respond_with(Cursor(self.clone()))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/uploads"))
            .respond_with(Upload(self.clone()))
            .mount(server)
            .await;
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap()
    }

    /// The next cursor reads get these answers (`None`: the stub's own).
    pub fn script_cursor(&self, replies: Vec<Option<ResponseTemplate>>) {
        self.lock().cursor_script.extend(replies);
    }

    /// The next Uploads get these answers (`None`: the stub's own).
    pub fn script_uploads(&self, replies: Vec<Option<ResponseTemplate>>) {
        self.lock().upload_script.extend(replies);
    }

    /// Every page received, answered or not.
    pub fn pages(&self) -> Vec<Seen> {
        self.lock().pages.clone()
    }

    pub fn cursor_reads(&self) -> usize {
        self.lock().cursor_reads
    }

    /// The `seq` of the held end of `log_id`.
    pub fn end(&self, log_id: &str) -> u64 {
        self.lock()
            .logs
            .get(log_id)
            .map_or(0, |held| held.entries.len() as u64)
    }

    /// The Player deletes the Catch log on the website (UPL-R60): a
    /// tombstone, and the held copy is gone.
    pub fn delete(&self, log_id: &str) {
        let mut inner = self.lock();
        inner.logs.remove(log_id);
        inner.tombstones.insert(log_id.to_owned());
    }

    /// The Player allows it again (UPL-R63).
    pub fn allow(&self, log_id: &str) {
        self.lock().tombstones.remove(log_id);
    }

    /// The `catchLog` later successes carry (UPL-R39, UPL-R44).
    pub fn set_catch_log(&self, catch_log: serde_json::Value) {
        self.lock().catch_log = Some(catch_log);
    }

    /// Holds a Catch log made by applying `document` (an Upload's
    /// decompressed bytes), as if it had been uploaded.
    pub fn hold(&self, document: &[u8]) {
        let (_, body) = apply(&mut self.lock(), document);
        assert_eq!(body["result"], "applied", "{body}");
    }
}

struct Cursor(FakePagle);

impl Respond for Cursor {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut inner = self.0.lock();
        inner.cursor_reads += 1;
        if let Some(Some(reply)) = inner.cursor_script.pop_front() {
            return reply;
        }
        let log_id = request.url.path().split('/').nth(4).unwrap_or_default();
        let (end, newest) = match inner.logs.get(log_id) {
            Some(held) if !held.entries.is_empty() => (
                json!({ "seq": held.entries.len(), "entryId": held.entries.last().unwrap().id }),
                json!(held.newest_as_of),
            ),
            _ => (json!({ "seq": 0, "entryId": null }), json!(null)),
        };
        ResponseTemplate::new(200)
            .set_body_json(json!({ "logId": log_id, "end": end, "newestAsOf": newest }))
    }
}

struct Upload(FakePagle);

impl Respond for Upload {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        assert_eq!(
            request.headers.get("content-type").unwrap(),
            "application/vnd.pagle.upload+deflate"
        );
        let mut document = Vec::new();
        DeflateDecoder::new(request.body.as_slice())
            .read_to_end(&mut document)
            .expect("raw DEFLATE");
        let mut inner = self.0.lock();
        inner.pages.push(header(&document));
        if let Some(Some(reply)) = inner.upload_script.pop_front() {
            return reply;
        }
        let (status, body) = apply(&mut inner, &document);
        ResponseTemplate::new(status).set_body_json(body)
    }
}

fn field(fields: &[&str], i: usize) -> u64 {
    fields[i].parse().unwrap()
}

/// `PGL|1|logId|format|as-of|total|characters|from|anchor|count|bytes|crc`.
fn header(document: &[u8]) -> Seen {
    let text = std::str::from_utf8(document).unwrap();
    let line = text.split('\n').next().unwrap();
    let fields: Vec<&str> = line.split('|').collect();
    assert_eq!(fields[0], "PGL");
    Seen {
        log_id: fields[2].to_owned(),
        as_of: field(&fields, 4),
        total: field(&fields, 5),
        from: field(&fields, 7),
        anchor: fields[8].to_owned(),
        count: field(&fields, 9),
    }
}

/// Applies an Upload (UPL-R30 to R36 and UPL-R64, without the caps).
fn apply(inner: &mut Inner, document: &[u8]) -> (u16, serde_json::Value) {
    let seen = header(document);
    let text = std::str::from_utf8(document).unwrap();
    let fields: Vec<&str> = text.split('\n').next().unwrap().split('|').collect();
    let characters_count = field(&fields, 6) as usize;
    let lines: Vec<&str> = text.split('\n').skip(1).collect();
    let characters: Vec<String> = lines[..characters_count]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let entries: Vec<Entry> = lines[characters_count..characters_count + seen.count as usize]
        .iter()
        .map(|line| {
            let parts: Vec<&str> = line.splitn(5, '|').collect();
            let char: usize = parts[1].parse().unwrap();
            let guid = characters[char - 1].split('|').next().unwrap();
            Entry {
                id: format!("{guid}-{}-{}", parts[2], parts[3]),
                bytes: line.as_bytes().to_vec(),
            }
        })
        .collect();

    if inner.tombstones.contains(&seen.log_id) {
        return (
            409,
            json!({ "result": "refused", "reason": "deleted",
                    "message": "You deleted this Catch log on the Pagle website. Allow it again there to upload it.",
                    "logId": seen.log_id, "end": { "seq": 0, "entryId": null },
                    "newestAsOf": null, "catchLog": null }),
        );
    }
    let catch_log = inner.catch_log.clone().unwrap_or_else(forever_api);
    let held = inner.logs.entry(seen.log_id.clone()).or_default();
    let e = held.entries.len() as u64;
    let refuse = |status: u16, code: &str, held: &Held| {
        (
            status,
            json!({ "result": "refused", "reason": code, "message": code, "logId": seen.log_id,
                    "end": end(held), "newestAsOf": held.newest_as_of, "catchLog": null }),
        )
    };
    if seen.from > e + 1 {
        return refuse(409, "gap", held);
    }
    if seen.from > 1 && held.entries[(seen.from - 2) as usize].id != seen.anchor {
        return refuse(409, "divergence", held);
    }
    let f = (seen.from - 1) as usize;
    let mut result: Vec<Entry> = held.entries[..f].to_vec();
    result.extend(entries);
    let keep_to = (e.min(seen.total)) as usize;
    if keep_to > result.len() {
        result.extend_from_slice(&held.entries[result.len()..keep_to]);
    }
    let unchanged = result == held.entries && characters == held.characters;
    if !unchanged && seen.as_of < held.newest_as_of {
        return refuse(409, "stale", held);
    }
    if !unchanged {
        held.entries = result;
        held.characters = characters;
        held.newest_as_of = held.newest_as_of.max(seen.as_of);
    }
    (
        200,
        json!({ "result": if unchanged { "unchanged" } else { "applied" }, "reason": null,
                "message": null, "logId": seen.log_id, "end": end(held),
                "newestAsOf": held.newest_as_of, "catchLog": catch_log }),
    )
}

fn end(held: &Held) -> serde_json::Value {
    match held.entries.last() {
        Some(last) => json!({ "seq": held.entries.len(), "entryId": last.id }),
        None => json!({ "seq": 0, "entryId": null }),
    }
}

/// One in-game download as [`FakeInGame`] received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub file: DataFile,
    pub accept_encoding: Option<String>,
    pub if_none_match: Option<String>,
    pub authorization: Option<String>,
    pub version: Option<String>,
}

#[derive(Default)]
struct InGameInner {
    bodies: HashMap<DataFile, Vec<u8>>,
    scripts: HashMap<DataFile, VecDeque<Option<ResponseTemplate>>>,
    received: Vec<Download>,
    /// Whether each `200` the stub answered itself went out gzip-encoded.
    gzipped: Vec<bool>,
}

/// A stub of the in-game downloads (IGV Contracts, Endpoints; HEV Contracts,
/// Endpoints): it answers each with the body a test set (the Contracts
/// examples, and the test vector's `good` file for Content, at first) as
/// IGV-R34 says, `text/x-lua` and gzip-encoded when `Accept-Encoding` allows
/// gzip; the Anglers and Content bodies with their `ETag` (IGV-R32,
/// HEV-R37), and a `304` when `If-None-Match` matches it. Tests can make it answer something else for
/// the next requests of an endpoint and read every request it received.
#[derive(Clone, Default)]
pub struct FakeInGame(Arc<Mutex<InGameInner>>);

impl FakeInGame {
    pub async fn mount(&self, server: &MockServer) {
        for file in DataFile::ALL {
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/{}", endpoint(file))))
                .respond_with(InGameEndpoint(self.clone(), file))
                .mount(server)
                .await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, InGameInner> {
        self.0.lock().unwrap()
    }

    /// The body `file`'s download answers from now on.
    pub fn set_body(&self, file: DataFile, body: impl Into<Vec<u8>>) {
        self.lock().bodies.insert(file, body.into());
    }

    /// The body `file`'s download answers.
    pub fn body(&self, file: DataFile) -> Vec<u8> {
        let inner = self.lock();
        match inner.bodies.get(&file) {
            Some(body) => body.clone(),
            None => default_body(file),
        }
    }

    /// The next downloads of `file` get these answers (`None`: the stub's
    /// own).
    pub fn script(&self, file: DataFile, replies: Vec<Option<ResponseTemplate>>) {
        self.lock().scripts.entry(file).or_default().extend(replies);
    }

    /// Every download received, in order.
    pub fn received(&self) -> Vec<Download> {
        self.lock().received.clone()
    }

    /// The downloads of `file` received.
    pub fn received_of(&self, file: DataFile) -> Vec<Download> {
        self.received()
            .into_iter()
            .filter(|d| d.file == file)
            .collect()
    }

    /// Whether each `200` the stub answered itself was gzip-encoded.
    pub fn gzipped(&self) -> Vec<bool> {
        self.lock().gzipped.clone()
    }
}

fn default_body(file: DataFile) -> Vec<u8> {
    match file {
        DataFile::Following => FOLLOWING_EXAMPLE.into(),
        DataFile::Anglers => ANGLERS_EXAMPLE.into(),
        DataFile::Content => content_vector::content_file("good").into_bytes(),
    }
}

/// `bytes` gzip-encoded, as the backend sends a data file (IGV-R34).
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

struct InGameEndpoint(FakeInGame, DataFile);

impl Respond for InGameEndpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let header = |name: &str| {
            request
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let file = self.1;
        let seen = Download {
            file,
            accept_encoding: header("accept-encoding"),
            if_none_match: header("if-none-match"),
            authorization: header("authorization"),
            version: header("pagle-companion-version"),
        };
        let body = self.0.body(file);
        let mut inner = self.0.lock();
        inner.received.push(seen.clone());
        if let Some(Some(reply)) = inner.scripts.get_mut(&file).and_then(VecDeque::pop_front) {
            return reply;
        }
        let etag = etag_of(&body);
        let mut reply = ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/x-lua; charset=utf-8")
            .insert_header("Cache-Control", "private, no-cache")
            .insert_header("Vary", "Accept-Encoding");
        if file != DataFile::Following {
            if seen.if_none_match.as_deref() == Some(etag.as_str()) {
                return ResponseTemplate::new(304)
                    .insert_header("ETag", etag.as_str())
                    .insert_header("Cache-Control", "private, no-cache")
                    .insert_header("Vary", "Accept-Encoding");
            }
            reply = reply.insert_header("ETag", etag.as_str());
        }
        let gzip_allowed = seen
            .accept_encoding
            .as_deref()
            .is_some_and(|v| v.split(',').any(|c| c.trim() == "gzip"));
        inner.gzipped.push(gzip_allowed);
        if gzip_allowed {
            reply
                .insert_header("Content-Encoding", "gzip")
                .set_body_bytes(gzip(&body))
        } else {
            reply.set_body_bytes(body)
        }
    }
}
