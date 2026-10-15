//! The link between this Companion and a Player (CMP-R5, CMP-R49, CMP-R61 to
//! CMP-R65): the one credential in its store, the requests that carry it,
//! rotation, storing a new link, Unlink, Link again… and losing the link.
//!
//! Every request to an endpoint that takes the credential goes through
//! [`Account::send`]: it sends nothing while not linked (CMP-R55) or while
//! the keyring is locked (CMP-R62), reads the credential before each request,
//! adds `Authorization: Bearer`, stores a `Pagle-New-Credential` successor
//! before it is ever used, and turns a `credential_invalid` reply into a lost
//! link (CMP-R65). Config reads, update checks and the device flow use
//! `Api::request` directly and carry no credential.
//!
//! What the queue (#28) and the data addons (B4) do on these events plugs in
//! through [`Hooks`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::header::HeaderName;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;

use crate::api::Api;
use crate::credential_store::{Keychain, StoreError};
use crate::device_flow::Approval;
use crate::notify::{Cause, CauseKey, Notifications, Notifier};
use crate::state::{InGame, Store};

/// Carries the successor credential on a `2xx` (ACC-R52).
pub const NEW_CREDENTIAL: HeaderName = HeaderName::from_static("pagle-new-credential");

/// Revoke timeout (CMP Tunables): self-revoke gets one attempt within it.
pub const REVOKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a locked keyring is read again: the poll interval (CMP Tunables,
/// CMP-R62).
pub const KEYRING_RETRY: Duration = Duration::from_secs(30);

/// What the window shows while the keyring holding the credential doesn't
/// answer (CMP-R62).
pub const KEYRING_LOCKED_TEXT: &str =
    "Pagle Companion can't open your keyring. Unlock it to keep uploading.";

/// The refusal code of a lost link (ACC-R51).
pub const CREDENTIAL_INVALID: &str = "credential_invalid";

pub type Hook = Box<dyn Fn() + Send + Sync>;

/// What other parts of the Companion do when the link changes. Each defaults
/// to doing nothing until the ticket that owns it fills it in.
pub struct Hooks {
    /// Cancels every queued and running item of the queue (CMP-R63 step 1,
    /// CMP-R64, CMP-R65). The queue is the upload runs' (#28).
    pub cancel_queue: Hook,
    /// Starts the post-link runs (CMP-R101) after any link (#28).
    pub start_post_link_runs: Hook,
    /// Deletes both data addon folders in every install (CMP-R85), on Unlink
    /// and on losing the link (B4).
    pub delete_data_addon_folders: Hook,
    /// The link's status changed: the window and the tray show it again.
    pub changed: Hook,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            cancel_queue: Box::new(|| {}),
            start_post_link_runs: Box::new(|| {}),
            delete_data_addon_folders: Box::new(|| {}),
            changed: Box::new(|| {}),
        }
    }
}

/// The process's link: its stores and hooks. Reach it with
/// [`Link::account`].
pub struct Link {
    keychain: Keychain,
    hooks: Hooks,
    keyring_locked: AtomicBool,
}

impl Link {
    pub fn new(keychain: Keychain, hooks: Hooks) -> Self {
        Self {
            keychain,
            hooks,
            keyring_locked: AtomicBool::new(false),
        }
    }

    /// Whether the last read found the keyring not answering (CMP-R62).
    pub fn keyring_locked(&self) -> bool {
        self.keyring_locked.load(Ordering::SeqCst)
    }

    /// The link together with the parts of the Companion it works with.
    pub fn account<'a, N: Notifier>(
        &'a self,
        api: &'a Api,
        state: &'a Store,
        notifications: &'a Notifications<N>,
    ) -> Account<'a, N> {
        Account {
            link: self,
            api,
            state,
            notifications,
        }
    }
}

/// What the window's Status part shows of the link (CMP UI, Status).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkStatus {
    pub linked: bool,
    /// "Linked as {BattleTag}", "Linked" or "Not linked".
    pub text: String,
    /// **Link to Pagle** rather than **Link again…** and **Unlink**: no
    /// BattleTag is stored, or the store answered and holds no credential
    /// (CMP-R62).
    pub offer_link: bool,
    /// The keyring holding the credential doesn't answer (CMP-R62).
    pub keyring_locked: bool,
}

/// Why a request needing the credential got no reply to use.
#[derive(Debug)]
pub enum SendError {
    /// Not linked: nothing was sent (CMP-R55).
    NotLinked,
    /// The keyring doesn't answer: nothing was sent (CMP-R62).
    KeyringLocked,
    /// The store failed otherwise: nothing was sent.
    Store(StoreError),
    /// The reply was `credential_invalid`: this Companion is no longer
    /// linked, and CMP-R65 has run. The run or download ends, leaving
    /// `lastRun` as it was (CMP-R50).
    CredentialInvalid,
    /// No reply: a connection error or timeout.
    Http(reqwest::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NotLinked => f.write_str("not linked"),
            SendError::KeyringLocked => f.write_str("the keyring doesn't answer"),
            SendError::Store(e) => write!(f, "{e}"),
            SendError::CredentialInvalid => f.write_str("credential_invalid"),
            SendError::Http(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SendError {}

/// The link at work: its requests and changes.
pub struct Account<'a, N: Notifier> {
    link: &'a Link,
    api: &'a Api,
    state: &'a Store,
    notifications: &'a Notifications<N>,
}

impl<N: Notifier> Account<'_, N> {
    /// Whether a link is stored: a BattleTag is (CMP Data, `battleTag` is
    /// null while not linked).
    pub fn is_linked(&self) -> bool {
        self.state.read(|s| s.battle_tag.is_some())
    }

    /// The Status part's view, reading the store (CMP-R62).
    pub fn status(&self) -> LinkStatus {
        let not_linked = LinkStatus {
            linked: false,
            text: "Not linked".into(),
            offer_link: true,
            keyring_locked: false,
        };
        let Some(battle_tag) = self.state.read(|s| s.battle_tag.clone()) else {
            return not_linked;
        };
        // A store that doesn't answer may still hold the credential: the
        // Companion stays linked and says why it sends nothing.
        if let Ok(None) = self.read_credential() {
            return not_linked;
        }
        LinkStatus {
            linked: true,
            text: if battle_tag.is_empty() {
                "Linked".into()
            } else {
                format!("Linked as {battle_tag}")
            },
            offer_link: false,
            keyring_locked: self.link.keyring_locked.load(Ordering::SeqCst),
        }
    }

    /// Reads the credential from the store `credentialStore` names, noting
    /// whether the keyring answered (CMP-R62).
    fn read_credential(&self) -> Result<Option<String>, StoreError> {
        let kind = self.state.read(|s| s.credential_store);
        let read = self.link.keychain.current(kind).read();
        let locked = matches!(read, Err(StoreError::Unavailable(_)));
        if self.link.keyring_locked.swap(locked, Ordering::SeqCst) != locked {
            if locked {
                log::warn!("the keyring holding the credential doesn't answer");
            } else {
                log::info!("the keyring holding the credential answers again");
            }
            (self.link.hooks.changed)();
        }
        if let Err(StoreError::Failed(e)) = &read {
            log::error!("couldn't read the credential: {e}");
        }
        read
    }

    /// Whether the store still holds `credential`.
    fn holds(&self, credential: &str) -> bool {
        matches!(self.read_credential(), Ok(Some(c)) if c == credential)
    }

    /// Stores a `Pagle-New-Credential` successor in the same store; the next
    /// request reads it from there, so it is used only once written
    /// (CMP-R49).
    fn rotate(&self, current: &str, reply: &Response) {
        let Some(successor) = reply
            .headers()
            .get(NEW_CREDENTIAL)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != current)
        else {
            return;
        };
        // A link made meanwhile owns the store now.
        if !self.holds(current) {
            return;
        }
        let kind = self.state.read(|s| s.credential_store);
        match self.link.keychain.current(kind).write(successor) {
            Ok(()) => log::info!("stored the successor credential"),
            Err(e) => {
                log::warn!("couldn't store the successor credential; keeping the current one: {e}")
            }
        }
    }

    /// Self-revoke (ACC-R57): one attempt within the revoke timeout; a
    /// failure is ignored, since the credential expires unused (CMP-R63
    /// step 2).
    async fn self_revoke(&self, credential: &str) {
        let sent = self
            .api
            .request(Method::DELETE, "companion/credential")
            .bearer_auth(credential)
            .timeout(REVOKE_TIMEOUT)
            .send()
            .await;
        match sent {
            Ok(reply) => log::info!("self-revoke answered {}", reply.status()),
            Err(e) => log::warn!("self-revoke failed: {e}"),
        }
    }

    /// CMP-R63 step 3: deletes the credential from its store, the BattleTag
    /// (with `inGame`, CMP Data) and the data addon folders.
    fn forget(&self) {
        let kind = self.state.read(|s| s.credential_store);
        if let Err(e) = self.link.keychain.current(kind).delete() {
            log::error!("couldn't delete the credential: {e}");
        }
        (self.link.hooks.delete_data_addon_folders)();
        let saved = self.state.update(|s| {
            s.battle_tag = None;
            s.in_game = InGame::default();
        });
        if let Err(e) = saved {
            log::error!("couldn't write state.json: {e}");
        }
        self.link.keyring_locked.store(false, Ordering::SeqCst);
    }

    /// Sends a request to an endpoint that takes the credential (CMP-R49).
    /// `build` adds the body and headers; this adds the version header (via
    /// `Api`) and `Authorization: Bearer`, handles rotation and
    /// `credential_invalid`, and hands back every other reply as it came.
    pub async fn send<B>(&self, method: Method, path: &str, build: B) -> Result<Response, SendError>
    where
        B: FnOnce(RequestBuilder) -> RequestBuilder,
    {
        if !self.is_linked() {
            return Err(SendError::NotLinked);
        }
        let credential = match self.read_credential() {
            Ok(Some(credential)) => credential,
            Ok(None) => return Err(SendError::NotLinked),
            Err(StoreError::Unavailable(_)) => return Err(SendError::KeyringLocked),
            Err(e) => return Err(SendError::Store(e)),
        };
        let reply = build(self.api.request(method, path))
            .bearer_auth(&credential)
            .send()
            .await
            .map_err(SendError::Http)?;
        let status = reply.status();
        if status.is_success() {
            self.rotate(&credential, &reply);
            return Ok(reply);
        }
        if status != StatusCode::UNAUTHORIZED {
            return Ok(reply);
        }
        let (code, reply) = refusal_code(reply).await.map_err(SendError::Http)?;
        if code.as_deref() != Some(CREDENTIAL_INVALID) {
            return Ok(reply);
        }
        // A reply to a credential a new link has replaced loses nothing.
        if self.holds(&credential) {
            self.lose_link();
        }
        Err(SendError::CredentialInvalid)
    }

    /// Stores an approved link (CMP-R61), and after **Link again…** cancels
    /// the queue and self-revokes the old credential (CMP-R64); then starts
    /// the post-link runs. A failure is CMP-R61's text, and the credential is
    /// dropped.
    pub async fn store_link(&self, approval: Approval) -> Result<(), String> {
        let (was_linked, old_kind, game_mode) = self.state.read(|s| {
            (
                s.battle_tag.is_some(),
                s.credential_store,
                s.settings.game_mode,
            )
        });
        let old_store = self.link.keychain.current(old_kind);
        let old = if was_linked {
            old_store.read().ok().flatten()
        } else {
            None
        };
        let kind = self
            .link
            .keychain
            .store_new(&approval.credential, game_mode)
            .map_err(|place| {
                format!("Pagle Companion couldn't save its link in {place}. Link again.")
            })?;
        if was_linked && kind != old_kind {
            // The credential moved: the new store holds it, so the old one
            // lets go.
            if let Err(e) = old_store.delete() {
                log::warn!("couldn't delete the credential from its old store: {e}");
            }
        }
        let saved = self.state.update(|s| {
            s.battle_tag = Some(approval.battle_tag.clone());
            s.credential_store = kind;
            s.in_game = InGame::default();
            if was_linked {
                // CMP UI, Catch logs: Link again… clears the catchLog kept.
                for log in s.logs.values_mut() {
                    if let Some(run) = &mut log.last_run {
                        run.catch_log = None;
                    }
                }
            }
        });
        if let Err(e) = saved {
            log::error!("couldn't write state.json: {e}");
        }
        self.link.keyring_locked.store(false, Ordering::SeqCst);
        self.notifications.clear(&CauseKey::CredentialInvalid);
        log::info!("linked to a Player");
        if was_linked {
            (self.link.hooks.cancel_queue)();
            if let Some(old) = old.filter(|old| *old != approval.credential) {
                self.self_revoke(&old).await;
            }
        }
        (self.link.hooks.start_post_link_runs)();
        (self.link.hooks.changed)();
        Ok(())
    }

    /// **Unlink** after its confirmation (CMP-R63): cancels the queue,
    /// self-revokes once within the revoke timeout, and deletes the
    /// credential, the BattleTag and the data addon folders. Answers and
    /// folder picks stay.
    pub async fn unlink(&self) {
        log::info!("unlinking");
        (self.link.hooks.cancel_queue)();
        if let Ok(Some(credential)) = self.read_credential() {
            self.self_revoke(&credential).await;
        }
        self.forget();
        (self.link.hooks.changed)();
    }

    /// No longer linked (CMP-R65): CMP-R63 steps 1 and 3, and the
    /// notification.
    pub fn lose_link(&self) {
        log::warn!("the credential was refused as credential_invalid: no longer linked");
        (self.link.hooks.cancel_queue)();
        self.forget();
        self.notifications.raise(&Cause::CredentialInvalid);
        (self.link.hooks.changed)();
    }
}

/// The refusal code of a reply: its body's `reason`, `code` or `error`,
/// whichever is present (CMP-R50). The reply comes back whole.
async fn refusal_code(reply: Response) -> Result<(Option<String>, Response), reqwest::Error> {
    let status = reply.status();
    let version = reply.version();
    let headers = reply.headers().clone();
    let body = reply.bytes().await?;
    let code = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| {
            ["reason", "code", "error"]
                .iter()
                .find_map(|key| json.get(key)?.as_str().map(str::to_owned))
        });
    let mut rebuilt = http::Response::new(body);
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    Ok((code, Response::from(rebuilt)))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    use wiremock::{MockServer, ResponseTemplate};

    use super::*;
    use crate::credential_store::CredentialStore as _;
    use crate::credential_store::testing::MemoryStore;
    use crate::state::{self, Answer, CredentialStore as StoreKind, LogState, State};
    use crate::stub_server::{self, refusal};

    const OLD: &str = "pgc_old_credential";
    const NEW: &str = "pgc_new_credential";

    #[derive(Default, Clone)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Notifier for Recorder {
        fn show(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }
    }

    #[derive(Default)]
    struct Calls {
        cancel_queue: AtomicUsize,
        post_link: AtomicUsize,
        delete_folders: AtomicUsize,
        changed: AtomicUsize,
    }

    impl Calls {
        fn get(counter: &AtomicUsize) -> usize {
            counter.load(Ordering::SeqCst)
        }
    }

    struct Harness {
        tmp: tempfile::TempDir,
        server: MockServer,
        api: Api,
        os: MemoryStore,
        file: MemoryStore,
        link: Link,
        state: Store,
        notifications: Notifications<Recorder>,
        shown: Recorder,
        calls: Arc<Calls>,
    }

    impl Harness {
        /// Windows: Credential Manager and no credential file.
        async fn windows(state: State) -> Self {
            Self::build(state, false).await
        }

        /// Linux: the Secret Service and the credential file.
        async fn linux(state: State) -> Self {
            Self::build(state, true).await
        }

        async fn build(state: State, linux: bool) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let server = stub_server::start().await;
            let os = MemoryStore::new(if linux {
                "your keyring"
            } else {
                "Windows Credential Manager"
            });
            let file = MemoryStore::new("its settings folder");
            let keychain = Keychain::new(
                Box::new(os.clone()),
                linux.then(|| Box::new(file.clone()) as Box<_>),
            );
            let calls = Arc::new(Calls::default());
            let hooks = {
                let (a, b, c, d) = (calls.clone(), calls.clone(), calls.clone(), calls.clone());
                Hooks {
                    cancel_queue: Box::new(move || {
                        a.cancel_queue.fetch_add(1, Ordering::SeqCst);
                    }),
                    start_post_link_runs: Box::new(move || {
                        b.post_link.fetch_add(1, Ordering::SeqCst);
                    }),
                    delete_data_addon_folders: Box::new(move || {
                        c.delete_folders.fetch_add(1, Ordering::SeqCst);
                    }),
                    changed: Box::new(move || {
                        d.changed.fetch_add(1, Ordering::SeqCst);
                    }),
                }
            };
            let shown = Recorder::default();
            Self {
                api: stub_server::api(&server),
                state: Store::new(tmp.path().join("state.json"), state),
                tmp,
                server,
                os,
                file,
                link: Link::new(keychain, hooks),
                notifications: Notifications::new(shown.clone()),
                shown,
                calls,
            }
        }

        fn account(&self) -> Account<'_, Recorder> {
            self.link
                .account(&self.api, &self.state, &self.notifications)
        }

        fn state_json(&self) -> String {
            std::fs::read_to_string(self.tmp.path().join("state.json")).unwrap()
        }

        fn shown(&self) -> Vec<String> {
            self.shown.0.lock().unwrap().clone()
        }

        async fn bearers(&self, endpoint: &str) -> Vec<String> {
            stub_server::requests(&self.server, endpoint)
                .await
                .iter()
                .map(|r| {
                    r.headers
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .unwrap_or_default()
                })
                .collect()
        }
    }

    /// Local state after the first run, not linked, with one answered log.
    fn not_linked() -> State {
        let mut state = State {
            first_run_done: true,
            ..State::default()
        };
        state.logs.insert(
            "3f9a2c1e7b0d4a65".into(),
            LogState {
                answer: Some(Answer::Upload),
                folder: Some("G:/WoW/_classic_beta_/WTF/Account/1#1".into()),
                characters: "Ada Brook".into(),
                last_folder: "G:/WoW/_classic_beta_/WTF/Account/1#1".into(),
                whole_send_pending: false,
                last_run: None,
                last_uploaded_at: None,
            },
        );
        state
    }

    /// Linked as Ada#1234, with in-game download times.
    fn linked(kind: Option<StoreKind>) -> State {
        let mut state = not_linked();
        state.battle_tag = Some("Ada#1234".into());
        state.credential_store = kind;
        state.in_game.following_at = Some(1_791_735_000);
        state.in_game.anglers_etag = Some("\"9f86\"".into());
        state
    }

    fn approval(credential: &str, battle_tag: &str) -> Approval {
        Approval {
            credential: credential.into(),
            battle_tag: battle_tag.into(),
        }
    }

    #[tokio::test]
    async fn approval_stores_the_credential_and_shows_linked_as() {
        // CMP-R61: stores the credential (CMP-R62) and the BattleTag, shows
        // "Linked as {BattleTag}", and starts the post-link runs
        let h = Harness::windows(not_linked()).await;

        h.account()
            .store_link(approval(NEW, "Ada#1234"))
            .await
            .unwrap();

        assert_eq!(h.os.credential().as_deref(), Some(NEW));
        let status = h.account().status();
        assert_eq!(
            status,
            LinkStatus {
                linked: true,
                text: "Linked as Ada#1234".into(),
                offer_link: false,
                keyring_locked: false,
            }
        );
        assert_eq!(Calls::get(&h.calls.post_link), 1);
        assert_eq!(Calls::get(&h.calls.cancel_queue), 0);
        assert!(Calls::get(&h.calls.changed) >= 1);
        assert!(h.state_json().contains("\"battleTag\": \"Ada#1234\""));
    }

    #[tokio::test]
    async fn the_credential_appears_nowhere_but_its_store() {
        // CMP-R62: the credential never appears in local state or the window
        let h = Harness::windows(not_linked()).await;
        h.account()
            .store_link(approval(NEW, "Ada#1234"))
            .await
            .unwrap();

        assert!(!h.state_json().contains(NEW));
        let window = serde_json::to_string(&h.account().status()).unwrap();
        assert!(!window.contains(NEW), "{window}");
    }

    #[tokio::test]
    async fn an_empty_battle_tag_shows_linked() {
        // CMP-R61: "Linked" when the BattleTag is empty
        let h = Harness::windows(not_linked()).await;
        h.account().store_link(approval(NEW, "")).await.unwrap();
        assert_eq!(h.account().status().text, "Linked");
        assert!(h.account().is_linked());
    }

    #[tokio::test]
    async fn a_failed_store_drops_the_credential_and_says_so() {
        // CMP-R61: if storing fails, the credential is dropped and the window
        // says "Pagle Companion couldn't save its link in {store}. Link
        // again."
        let h = Harness::windows(not_linked()).await;
        h.os.fail_writes(Some(StoreError::Failed("denied".into())));

        let failure = h
            .account()
            .store_link(approval(NEW, "Ada#1234"))
            .await
            .unwrap_err();
        assert_eq!(
            failure,
            "Pagle Companion couldn't save its link in Windows Credential Manager. Link again."
        );
        assert!(!h.account().is_linked());
        assert_eq!(h.account().status().text, "Not linked");
        assert_eq!(Calls::get(&h.calls.post_link), 0);
    }

    #[tokio::test]
    async fn a_locked_keyring_at_link_time_keeps_the_credential_in_the_file() {
        // CMP-R62: on Linux with a locked keyring the file store is chosen at
        // link time, and credentialStore says so
        let h = Harness::linux(not_linked()).await;
        h.os.fail_writes(Some(StoreError::Unavailable("locked".into())));

        h.account()
            .store_link(approval(NEW, "Ada#1234"))
            .await
            .unwrap();
        assert_eq!(h.file.credential().as_deref(), Some(NEW));
        assert_eq!(h.state.read(|s| s.credential_store), Some(StoreKind::File));
        assert!(h.state_json().contains("\"credentialStore\": \"file\""));
    }

    #[tokio::test]
    async fn an_unlocked_keyring_at_link_time_keeps_the_credential_there() {
        // CMP-R62: the Secret Service when its default collection is already
        // unlocked and storing asks no prompt
        let h = Harness::linux(not_linked()).await;
        h.account()
            .store_link(approval(NEW, "Ada#1234"))
            .await
            .unwrap();
        assert_eq!(h.os.credential().as_deref(), Some(NEW));
        assert_eq!(
            h.state.read(|s| s.credential_store),
            Some(StoreKind::SecretService)
        );
    }

    #[tokio::test]
    async fn requests_carry_the_credential_as_a_bearer_token() {
        // CMP-R49: every request to an endpoint that takes a credential
        // carries Authorization: Bearer <credential>; DST-R60: and the
        // version header
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "GET",
            "catch-logs/3f9a2c1e7b0d4a65/cursor",
            vec![ResponseTemplate::new(200)],
        )
        .await;

        let reply = h
            .account()
            .send(Method::GET, "catch-logs/3f9a2c1e7b0d4a65/cursor", |r| r)
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        let sent = stub_server::requests(&h.server, "catch-logs/3f9a2c1e7b0d4a65/cursor").await;
        assert_eq!(
            sent[0].headers.get("authorization").unwrap(),
            &format!("Bearer {OLD}")
        );
        assert_eq!(
            sent[0].headers.get("pagle-companion-version").unwrap(),
            "1.0.0"
        );
    }

    #[tokio::test]
    async fn nothing_is_sent_while_not_linked() {
        // CMP-R55: nothing is sent while the Companion isn't linked
        let h = Harness::windows(not_linked()).await;
        h.os.write(OLD).unwrap();

        let sent = h.account().send(Method::POST, "uploads", |r| r).await;
        assert!(matches!(sent, Err(SendError::NotLinked)), "{sent:?}");
        assert!(stub_server::requests(&h.server, "uploads").await.is_empty());
    }

    #[tokio::test]
    async fn a_locked_keyring_sends_nothing_and_the_window_says_so() {
        // CMP-R62: with credentialStore secret-service and a locked keyring,
        // nothing needing the credential is sent and the window says so;
        // Link to Pagle isn't offered
        let h = Harness::linux(linked(Some(StoreKind::SecretService))).await;
        h.os.write(OLD).unwrap();
        h.os.fail_reads(Some(StoreError::Unavailable("locked".into())));

        let sent = h.account().send(Method::POST, "uploads", |r| r).await;
        assert!(matches!(sent, Err(SendError::KeyringLocked)), "{sent:?}");
        assert!(stub_server::requests(&h.server, "uploads").await.is_empty());
        let status = h.account().status();
        assert!(status.keyring_locked);
        assert!(status.linked);
        assert!(!status.offer_link);
        assert_eq!(
            KEYRING_LOCKED_TEXT,
            "Pagle Companion can't open your keyring. Unlock it to keep uploading."
        );

        // Read again once unlocked (CMP-R62: tried again every poll
        // interval).
        h.os.fail_reads(None);
        assert!(!h.account().status().keyring_locked);
        assert_eq!(KEYRING_RETRY, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn link_to_pagle_is_offered_when_the_store_holds_no_credential() {
        // CMP-R62: Link to Pagle is offered when no battleTag is stored, or
        // the store answered and holds no credential
        let h = Harness::windows(linked(None)).await;
        let status = h.account().status();
        assert!(status.offer_link);
        assert!(!status.linked);
        assert_eq!(status.text, "Not linked");
        let sent = h.account().send(Method::POST, "uploads", |r| r).await;
        assert!(matches!(sent, Err(SendError::NotLinked)), "{sent:?}");
    }

    #[tokio::test]
    async fn a_successor_replaces_the_credential_and_the_next_request_uses_it() {
        // CMP-R49: on Pagle-New-Credential the Companion writes the successor
        // to its store, and once that write succeeds uses it for every later
        // request
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "POST",
            "uploads",
            vec![
                ResponseTemplate::new(200).insert_header("Pagle-New-Credential", NEW),
                ResponseTemplate::new(200),
            ],
        )
        .await;

        h.account()
            .send(Method::POST, "uploads", |r| r)
            .await
            .unwrap();
        assert_eq!(h.os.credential().as_deref(), Some(NEW));
        h.account()
            .send(Method::POST, "uploads", |r| r)
            .await
            .unwrap();

        assert_eq!(
            h.bearers("uploads").await,
            [format!("Bearer {OLD}"), format!("Bearer {NEW}")]
        );
    }

    #[tokio::test]
    async fn a_failed_successor_write_keeps_the_old_credential() {
        // CMP-R49: if the write fails, it keeps using the current credential
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        h.os.fail_writes(Some(StoreError::Failed("denied".into())));
        stub_server::script(
            &h.server,
            "POST",
            "uploads",
            vec![
                ResponseTemplate::new(200).insert_header("Pagle-New-Credential", NEW),
                ResponseTemplate::new(200),
            ],
        )
        .await;

        h.account()
            .send(Method::POST, "uploads", |r| r)
            .await
            .unwrap();
        h.account()
            .send(Method::POST, "uploads", |r| r)
            .await
            .unwrap();

        assert_eq!(h.os.credential().as_deref(), Some(OLD));
        assert_eq!(
            h.bearers("uploads").await,
            [format!("Bearer {OLD}"), format!("Bearer {OLD}")]
        );
    }

    #[tokio::test]
    async fn only_a_2xx_rotates() {
        // CMP-R49: when a 2xx response carries Pagle-New-Credential
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "POST",
            "uploads",
            vec![refusal(422, "malformed").insert_header("Pagle-New-Credential", NEW)],
        )
        .await;

        let reply = h
            .account()
            .send(Method::POST, "uploads", |r| r)
            .await
            .unwrap();
        assert_eq!(reply.status(), 422);
        assert_eq!(h.os.credential().as_deref(), Some(OLD));
    }

    #[tokio::test]
    async fn the_successor_goes_to_the_same_store() {
        // CMP-R62: rotation always writes to the same store
        let h = Harness::linux(linked(Some(StoreKind::File))).await;
        h.file.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "POST",
            "uploads",
            vec![ResponseTemplate::new(204).insert_header("Pagle-New-Credential", NEW)],
        )
        .await;

        h.account()
            .send(Method::POST, "uploads", |r| r)
            .await
            .unwrap();
        assert_eq!(h.file.credential().as_deref(), Some(NEW));
        assert_eq!(h.os.writes(), 0);
    }

    #[tokio::test]
    async fn credential_invalid_unlinks_locally_and_notifies_once() {
        // CMP-R65: a credential_invalid reply on any request does CMP-R63
        // steps 1 and 3 and raises "This Companion is no longer linked. Link
        // it again." (once, CMP-R110); the window offers Link to Pagle
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "POST",
            "uploads",
            vec![ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "result": "refused", "reason": "credential_invalid", "message": "x"
            }))],
        )
        .await;
        stub_server::script(
            &h.server,
            "GET",
            "in-game/following",
            vec![refusal(401, CREDENTIAL_INVALID)],
        )
        .await;

        let sent = h.account().send(Method::POST, "uploads", |r| r).await;
        assert!(
            matches!(sent, Err(SendError::CredentialInvalid)),
            "{sent:?}"
        );

        assert_eq!(h.os.credential(), None);
        assert!(!h.account().is_linked());
        assert_eq!(h.state.read(|s| s.in_game.clone()), InGame::default());
        assert_eq!(Calls::get(&h.calls.cancel_queue), 1);
        assert_eq!(Calls::get(&h.calls.delete_folders), 1);
        // Answers and folder picks stay.
        let log = h.state.read(|s| s.logs["3f9a2c1e7b0d4a65"].clone());
        assert_eq!(log.answer, Some(Answer::Upload));
        assert!(log.folder.is_some());
        assert!(h.account().status().offer_link);
        assert_eq!(
            h.shown(),
            ["This Companion is no longer linked. Link it again."]
        );

        // A second credential_invalid in the same process notifies no more.
        h.os.write(OLD).unwrap();
        h.state
            .update(|s| s.battle_tag = Some("Ada#1234".into()))
            .unwrap();
        let sent = h
            .account()
            .send(Method::GET, "in-game/following", |r| r)
            .await;
        assert!(
            matches!(sent, Err(SendError::CredentialInvalid)),
            "{sent:?}"
        );
        assert_eq!(h.shown().len(), 1);
    }

    #[tokio::test]
    async fn a_401_without_credential_invalid_is_an_ordinary_reply() {
        // CMP-R50: a 4xx without a known code counts as any other 4xx
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "POST",
            "uploads",
            vec![ResponseTemplate::new(401).set_body_string("nope")],
        )
        .await;

        let reply = h
            .account()
            .send(Method::POST, "uploads", |r| r)
            .await
            .unwrap();
        assert_eq!(reply.status(), 401);
        assert_eq!(reply.text().await.unwrap(), "nope");
        assert!(h.account().is_linked());
        assert_eq!(h.os.credential().as_deref(), Some(OLD));
    }

    #[tokio::test]
    async fn credential_invalid_for_a_replaced_credential_keeps_the_new_link() {
        // CMP-R64: a reply to the old credential, revoked by Link again…,
        // doesn't unlink the new link
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        let os = h.os.clone();
        stub_server::script(
            &h.server,
            "POST",
            "uploads",
            vec![refusal(401, CREDENTIAL_INVALID)],
        )
        .await;

        let sent = h
            .account()
            .send(Method::POST, "uploads", move |r| {
                // The new link is stored while the request is under way.
                os.write(NEW).unwrap();
                r
            })
            .await;
        assert!(
            matches!(sent, Err(SendError::CredentialInvalid)),
            "{sent:?}"
        );
        assert_eq!(h.os.credential().as_deref(), Some(NEW));
        assert!(h.account().is_linked());
        assert!(h.shown().is_empty());
    }

    #[tokio::test]
    async fn unlink_self_revokes_once_and_forgets_the_link() {
        // CMP-R63: cancels the queue, self-revokes (one attempt), deletes the
        // credential, the BattleTag and the data addon folders; answers and
        // folder picks stay; CMP Data: Unlink clears battleTag and inGame
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "DELETE",
            "companion/credential",
            vec![ResponseTemplate::new(204)],
        )
        .await;

        h.account().unlink().await;

        assert_eq!(
            h.bearers("companion/credential").await,
            [format!("Bearer {OLD}")]
        );
        assert_eq!(h.os.credential(), None);
        assert!(!h.account().is_linked());
        assert_eq!(h.state.read(|s| s.in_game.clone()), InGame::default());
        assert_eq!(Calls::get(&h.calls.cancel_queue), 1);
        assert_eq!(Calls::get(&h.calls.delete_folders), 1);
        let log = h.state.read(|s| s.logs["3f9a2c1e7b0d4a65"].clone());
        assert_eq!(log.answer, Some(Answer::Upload));
        assert!(log.folder.is_some());
        assert!(h.shown().is_empty());
        let written = h.state_json();
        assert!(written.contains("\"battleTag\": null"), "{written}");
        assert!(written.contains("\"answer\": \"upload\""), "{written}");
    }

    #[tokio::test]
    async fn a_failed_self_revoke_is_ignored() {
        // CMP-R63 step 2: one attempt; a failure is ignored
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "DELETE",
            "companion/credential",
            vec![ResponseTemplate::new(503), ResponseTemplate::new(204)],
        )
        .await;

        h.account().unlink().await;
        assert_eq!(h.bearers("companion/credential").await.len(), 1);
        assert_eq!(h.os.credential(), None);
        assert!(!h.account().is_linked());
        assert_eq!(REVOKE_TIMEOUT, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn link_again_replaces_the_link_and_revokes_the_old_credential() {
        // CMP-R64: once the new credential is stored, cancels the queue,
        // self-revokes the old credential and starts the post-link runs;
        // answers stay; inGame is cleared
        let mut state = linked(None);
        state.logs.get_mut("3f9a2c1e7b0d4a65").unwrap().last_run = Some(state::LastRun {
            result: state::RunResult::UpToDate,
            at: 1,
            file_mtime_ms: 1,
            file_size: 1,
            format: 1,
            companion_version: "1.0.0".into(),
            code: None,
            message: None,
            catch_log: Some(state::CatchLogState {
                verification: "pending".into(),
                pending_reason: Some("forever-api".into()),
                held_characters: 0,
                left_off: None,
            }),
        });
        let h = Harness::windows(state).await;
        h.os.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "DELETE",
            "companion/credential",
            vec![ResponseTemplate::new(204)],
        )
        .await;

        h.account()
            .store_link(approval(NEW, "Kukli#2345"))
            .await
            .unwrap();

        assert_eq!(h.os.credential().as_deref(), Some(NEW));
        assert_eq!(h.account().status().text, "Linked as Kukli#2345");
        assert_eq!(
            h.bearers("companion/credential").await,
            [format!("Bearer {OLD}")]
        );
        assert_eq!(Calls::get(&h.calls.cancel_queue), 1);
        assert_eq!(Calls::get(&h.calls.post_link), 1);
        assert_eq!(h.state.read(|s| s.in_game.clone()), InGame::default());
        let log = h.state.read(|s| s.logs["3f9a2c1e7b0d4a65"].clone());
        assert_eq!(log.answer, Some(Answer::Upload));
        // CMP UI, Catch logs: Link again… clears the catchLog kept.
        assert_eq!(log.last_run.unwrap().catch_log, None);
    }

    #[tokio::test]
    async fn link_again_moves_the_credential_between_linux_stores() {
        // CMP-R62: a link chooses the store; moving writes the new store
        // first, then deletes from the old one
        let h = Harness::linux(linked(Some(StoreKind::File))).await;
        h.file.write(OLD).unwrap();
        stub_server::script(
            &h.server,
            "DELETE",
            "companion/credential",
            vec![ResponseTemplate::new(204)],
        )
        .await;

        h.account()
            .store_link(approval(NEW, "Ada#1234"))
            .await
            .unwrap();
        assert_eq!(h.os.credential().as_deref(), Some(NEW));
        assert_eq!(h.file.credential(), None);
        assert_eq!(
            h.state.read(|s| s.credential_store),
            Some(StoreKind::SecretService)
        );
        assert_eq!(
            h.bearers("companion/credential").await,
            [format!("Bearer {OLD}")]
        );
    }

    #[tokio::test]
    async fn linking_again_after_losing_the_link_notifies_again_next_time() {
        // CMP-R110: a cause notifies again only after it has cleared; a new
        // link clears credential_invalid
        let h = Harness::windows(linked(None)).await;
        h.os.write(OLD).unwrap();
        h.account().lose_link();
        h.account()
            .store_link(approval(NEW, "Ada#1234"))
            .await
            .unwrap();
        h.account().lose_link();
        assert_eq!(h.shown().len(), 2);
    }
}
