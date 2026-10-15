//! The window's Status and Linking parts (CMP UI) and the commands behind
//! them: **Link to Pagle** and **Link again…** run the device flow
//! (CMP-R60) in the background, whether or not the window stays open;
//! **Cancel** stops it; **Unlink** runs CMP-R63. The window learns of every
//! change through two events: `link-status` ([`LinkStatus`]) and `linking`
//! ([`LinkingView`]).

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::watch;

use crate::device_flow::{self, End, Failure};
use crate::link::{KEYRING_RETRY, LinkStatus, Stop};
use crate::tray::{self, Companion};

/// The Linking part (CMP UI, Linking).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "phase",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LinkingView {
    /// Nothing to show: no link under way.
    Idle,
    /// The device request is under way.
    Starting,
    /// The user code, the verification URL and the QR code of
    /// `verification_uri_complete`, waiting for approval.
    Waiting {
        user_code: String,
        verification_uri: String,
        qr_svg: Option<String>,
    },
    /// An outcome text of CMP-R60 or CMP-R61.
    Ended { text: String },
}

/// The device flow under way, if any. Each start is a new attempt; Cancel
/// and a new start end the attempt before them, whose later results are
/// dropped.
pub struct Linking {
    attempt: watch::Sender<u64>,
    flow: Mutex<Flow>,
}

struct Flow {
    view: LinkingView,
    /// `verification_uri_complete` while waiting.
    page: Option<String>,
}

impl Default for Linking {
    fn default() -> Self {
        Self {
            attempt: watch::channel(0).0,
            flow: Mutex::new(Flow {
                view: LinkingView::Idle,
                page: None,
            }),
        }
    }
}

impl Linking {
    /// Starts a new attempt, ending any before it. The receiver sees the
    /// attempt end.
    pub fn begin(&self) -> (u64, watch::Receiver<u64>) {
        self.attempt.send_modify(|n| *n += 1);
        let attempt = *self.attempt.borrow();
        self.set_flow(LinkingView::Starting, None);
        (attempt, self.attempt.subscribe())
    }

    /// Ends the attempt under way: polling stops (CMP-R60).
    pub fn cancel(&self) {
        self.attempt.send_modify(|n| *n += 1);
        self.set_flow(LinkingView::Idle, None);
    }

    pub fn is_current(&self, attempt: u64) -> bool {
        *self.attempt.borrow() == attempt
    }

    /// Shows `view` if `attempt` is still the one under way.
    pub fn show(&self, attempt: u64, view: LinkingView, page: Option<String>) -> bool {
        let mut flow = self.lock();
        if !self.is_current(attempt) {
            return false;
        }
        *flow = Flow { view, page };
        true
    }

    pub fn view(&self) -> LinkingView {
        self.lock().view.clone()
    }

    /// The page to open while waiting.
    pub fn page(&self) -> Option<String> {
        self.lock().page.clone()
    }

    fn set_flow(&self, view: LinkingView, page: Option<String>) {
        *self.lock() = Flow { view, page };
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Flow> {
        self.flow.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Waits `pause`, or answers `false` as soon as the attempt ends.
async fn wait_or_cancel(pause: Duration, mut ended: watch::Receiver<u64>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(pause) => true,
        _ = ended.changed() => false,
    }
}

/// Whether the Companion opens `url` in the browser: the backend's own
/// HTTPS pages, or any HTTP page in a debug build against a local backend.
pub fn may_open(url: &str) -> bool {
    url.starts_with("https://") || (cfg!(debug_assertions) && url.starts_with("http://"))
}

fn open_in_browser(url: &str) {
    if !may_open(url) {
        log::warn!("not opening a verification page that isn't HTTPS");
        return;
    }
    if let Err(e) = tauri_plugin_opener::open_url(url, None::<&str>) {
        log::warn!("couldn't open the browser: {e}");
    }
}

/// Reads the link's status and shows it in the window and the tray.
pub fn link_changed(app: &AppHandle) {
    let Some(companion) = app.try_state::<Companion>() else {
        return;
    };
    let status = companion.account().status();
    tray::show_link(app, &status);
    let _ = app.emit("link-status", status);
}

fn emit_linking(app: &AppHandle) {
    let _ = app.emit("linking", app.state::<Linking>().view());
}

/// Reads the credential at start, then again every poll interval while the
/// keyring doesn't answer (CMP-R62).
pub fn start(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        link_changed(&app);
        loop {
            std::thread::sleep(KEYRING_RETRY);
            let companion = app.state::<Companion>();
            if companion.link.keyring_locked() {
                // A read that answers again clears the notice itself.
                companion.account().status();
            }
        }
    });
}

#[tauri::command]
pub async fn link_status(app: AppHandle) -> LinkStatus {
    app.state::<Companion>().account().status()
}

#[tauri::command]
pub fn linking_view(linking: tauri::State<'_, Linking>) -> LinkingView {
    linking.view()
}

/// **Link to Pagle** and **Link again…** (CMP-R60, CMP-R64).
#[tauri::command]
pub fn start_link(app: AppHandle) -> Result<(), String> {
    let companion = app.state::<Companion>();
    if !companion.is_watcher() {
        // CMP-R2: the window of a process without the lock is read-only.
        return Err("Another Pagle Companion is running for this user.".into());
    }
    if companion.account().update_required() {
        // CMP UI, Status: disabled while update required (DST-R56 allows no
        // device flow).
        return Err(device_flow::UPDATE_REQUIRED_TEXT.into());
    }
    // CMP-R100: pressing Link to Pagle ends the first run's step 1, so local
    // state is written from here on and the link outlives the process. The
    // rest of step 1 (answers from the ticks, the autostart entry) is the
    // first-run window's (Track C).
    if let Err(e) = companion.state.update(|s| s.first_run_done = true) {
        log::error!("couldn't write state.json: {e}");
    }
    let (attempt, ended) = app.state::<Linking>().begin();
    emit_linking(&app);
    tauri::async_runtime::spawn(run_link(app, attempt, ended));
    Ok(())
}

async fn run_link(app: AppHandle, attempt: u64, ended: watch::Receiver<u64>) {
    let companion = app.state::<Companion>();
    let linking = app.state::<Linking>();
    let name = device_flow::device_name(&device_flow::host_name());
    log::info!("linking started");
    let code = match device_flow::start(&companion.api, &name).await {
        Ok(code) => code,
        Err(failure) => {
            log::info!("linking failed: {failure:?}");
            enter_update_required(&app, &failure);
            if linking.show(attempt, ended_view(failure.text()), None) {
                emit_linking(&app);
            }
            return;
        }
    };
    let waiting = LinkingView::Waiting {
        user_code: code.user_code.clone(),
        verification_uri: code.verification_uri.clone(),
        qr_svg: device_flow::qr_svg(&code.verification_uri_complete),
    };
    if !linking.show(
        attempt,
        waiting,
        Some(code.verification_uri_complete.clone()),
    ) {
        return;
    }
    emit_linking(&app);
    open_in_browser(&code.verification_uri_complete);

    let end = device_flow::poll(&companion.api, &code, |pause| {
        wait_or_cancel(pause, ended.clone())
    })
    .await;
    let view = match end {
        End::Cancelled => return,
        // Cancelled just as it was approved: the credential is dropped, and
        // its device row expires unused (ACC-R54).
        End::Approved(_) if !linking.is_current(attempt) => return,
        End::Approved(approval) => match companion.account().store_link(approval).await {
            Ok(()) => LinkingView::Idle,
            Err(text) => ended_view(text),
        },
        End::Failed(failure) => {
            log::info!("linking ended: {failure:?}");
            enter_update_required(&app, &failure);
            ended_view(failure.text())
        }
    };
    if linking.show(attempt, view, None) {
        emit_linking(&app);
    }
}

/// `update_required` on the device flow stops linking and enters update
/// required (CMP-R60, CMP-R50).
fn enter_update_required(app: &AppHandle, failure: &Failure) {
    if let Failure::UpdateRequired(message) = failure {
        app.state::<Companion>()
            .account()
            .enter_stop(Stop::UpdateRequired {
                message: message.clone(),
            });
    }
}

fn ended_view(text: String) -> LinkingView {
    LinkingView::Ended { text }
}

/// **Cancel** in the Linking part: stops polling (CMP-R60).
#[tauri::command]
pub fn cancel_link(app: AppHandle) {
    app.state::<Linking>().cancel();
    log::info!("linking cancelled");
    emit_linking(&app);
}

/// Opens `verification_uri_complete` again, for the link in the window.
#[tauri::command]
pub fn open_link_page(linking: tauri::State<'_, Linking>) {
    if let Some(page) = linking.page() {
        open_in_browser(&page);
    }
}

/// **Unlink**, once the window has asked "Unlink this Companion?" (CMP-R63).
#[tauri::command]
pub async fn unlink(app: AppHandle) -> Result<(), String> {
    let companion = app.state::<Companion>();
    if !companion.is_watcher() {
        return Err("Another Pagle Companion is running for this user.".into());
    }
    companion.account().unlink().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting() -> LinkingView {
        LinkingView::Waiting {
            user_code: "BCDF-GHJK".into(),
            verification_uri: "https://pagle.app/link".into(),
            qr_svg: None,
        }
    }

    #[test]
    fn cancel_ends_the_attempt_and_drops_its_results() {
        // CMP-R60: the Player can cancel, which stops polling
        let linking = Linking::default();
        let (attempt, ended) = linking.begin();
        assert_eq!(linking.view(), LinkingView::Starting);

        linking.cancel();
        assert!(ended.has_changed().unwrap());
        assert!(!linking.is_current(attempt));
        assert!(!linking.show(attempt, waiting(), None));
        assert_eq!(linking.view(), LinkingView::Idle);
    }

    #[test]
    fn a_new_start_replaces_the_one_under_way() {
        // CMP-R5: one link at a time; starting again ends the attempt before
        let linking = Linking::default();
        let (first, _) = linking.begin();
        let (second, _) = linking.begin();
        assert!(!linking.show(first, waiting(), None));
        assert!(linking.show(
            second,
            waiting(),
            Some("https://pagle.app/link?code=BCDF-GHJK".into())
        ));
        assert_eq!(linking.view(), waiting());
        assert_eq!(
            linking.page().as_deref(),
            Some("https://pagle.app/link?code=BCDF-GHJK")
        );
    }

    #[tokio::test]
    async fn waiting_stops_as_soon_as_the_attempt_ends() {
        // CMP-R60: Cancel stops polling without waiting out the interval
        let linking = Linking::default();
        let (_, ended) = linking.begin();
        let wait = tokio::spawn(wait_or_cancel(Duration::from_secs(600), ended));
        linking.cancel();
        assert!(!wait.await.unwrap());

        let (_, ended) = linking.begin();
        assert!(wait_or_cancel(Duration::from_millis(1), ended).await);
    }

    #[test]
    fn the_window_gets_the_code_url_and_qr_code_but_never_the_credential() {
        // CMP UI, Linking: the user code, "Go to {verification_uri}", the QR
        // code; CMP-R62: the credential never reaches the window
        let json = serde_json::to_value(LinkingView::Waiting {
            user_code: "BCDF-GHJK".into(),
            verification_uri: "https://pagle.app/link".into(),
            qr_svg: Some("<svg/>".into()),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "phase": "waiting",
                "userCode": "BCDF-GHJK",
                "verificationUri": "https://pagle.app/link",
                "qrSvg": "<svg/>"
            })
        );
        assert_eq!(
            serde_json::to_value(ended_view("Linking was cancelled.".into())).unwrap(),
            serde_json::json!({ "phase": "ended", "text": "Linking was cancelled." })
        );
    }

    #[test]
    fn only_web_pages_are_opened() {
        // CMP-R60: opens verification_uri_complete in the default browser
        assert!(may_open("https://pagle.app/link?code=BCDF-GHJK"));
        assert!(!may_open("file:///C:/Windows/System32/calc.exe"));
        assert!(!may_open("javascript:alert(1)"));
    }
}
