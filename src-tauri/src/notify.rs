//! Notifications (CMP-R110, CMP-R111): raised in tray mode only when the
//! Player must act, each once per cause per process, and again only after
//! that cause has cleared. A successful Upload never notifies.
//!
//! While the first-run window is open, no notification is shown (CMP-R100):
//! [`Notifications::hold`] keeps every cause raised meanwhile, and
//! [`Notifications::release`] shows those that haven't cleared since. Every
//! cause goes through [`Notifications::raise`], so every cause is held.

use std::collections::HashSet;
use std::sync::Mutex;

/// Why the Companion needs the Player (CMP-R111), and the updater's two
/// notifications, `companion/distribution.md`'s: update required (DST-R57)
/// and the nudge (DST-R78).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A `logId` awaiting an answer (CMP-R40). `characters` is CMP-R40's list.
    AwaitingAnswer { log_id: String, characters: String },
    /// One `logId` in several account folders (CMP-R42).
    SeveralFolders { log_id: String, folders: usize },
    /// A divergence (CMP-R52).
    Divergence { log_id: String },
    /// A `logId` with `heldCharacters > 0` (CMP-R53).
    HeldCharacters { log_id: String },
    /// A `log_cap` refusal (CMP-R50), with the refusal's message.
    LogCap { message: String },
    /// A `credential_invalid` reply (CMP-R65).
    CredentialInvalid,
    /// The suspended stop (CMP-R50), with the refusal's message.
    Suspended { message: String },
    /// Update required, with DST-R57's picked text. Never cleared, so it
    /// shows at most once per process run, however often the state is left
    /// and entered (DST-R57, DST-R96).
    UpdateRequired { text: String },
    /// The running version is below `recommendedCompanionVersion` and an
    /// update is waiting: the nudge (DST-R78). Never cleared, so it
    /// shows at most once per process run.
    UpdateRecommended,
}

/// The nudge's notification (DST-R78).
pub const UPDATE_RECOMMENDED_TEXT: &str =
    "An important update for Pagle Companion is ready. Choose Restart to update in its tray menu.";

/// What makes two raises the same cause: the kind, and the `logId` for causes
/// that belong to one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CauseKey {
    AwaitingAnswer(String),
    SeveralFolders(String),
    Divergence(String),
    HeldCharacters(String),
    LogCap,
    CredentialInvalid,
    Suspended,
    UpdateRequired,
    UpdateRecommended,
}

impl Cause {
    pub fn key(&self) -> CauseKey {
        match self {
            Cause::AwaitingAnswer { log_id, .. } => CauseKey::AwaitingAnswer(log_id.clone()),
            Cause::SeveralFolders { log_id, .. } => CauseKey::SeveralFolders(log_id.clone()),
            Cause::Divergence { log_id } => CauseKey::Divergence(log_id.clone()),
            Cause::HeldCharacters { log_id } => CauseKey::HeldCharacters(log_id.clone()),
            Cause::LogCap { .. } => CauseKey::LogCap,
            Cause::CredentialInvalid => CauseKey::CredentialInvalid,
            Cause::Suspended { .. } => CauseKey::Suspended,
            Cause::UpdateRequired { .. } => CauseKey::UpdateRequired,
            Cause::UpdateRecommended => CauseKey::UpdateRecommended,
        }
    }

    /// The notification's text (CMP-R111).
    pub fn text(&self) -> String {
        match self {
            Cause::AwaitingAnswer { characters, .. } => {
                format!("Found a Catch log: {characters}. Upload it?")
            }
            Cause::SeveralFolders { folders, .. } => format!(
                "The same Catch log is in {folders} account folders. Pick the one you play on."
            ),
            Cause::Divergence { .. } => "This Catch log differs from the copy on Pagle.".into(),
            Cause::HeldCharacters { .. } => {
                "A new character needs verifying. Sign in again to verify it.".into()
            }
            Cause::LogCap { message } | Cause::Suspended { message } => message.clone(),
            Cause::UpdateRequired { text } => text.clone(),
            Cause::UpdateRecommended => UPDATE_RECOMMENDED_TEXT.into(),
            Cause::CredentialInvalid => "This Companion is no longer linked. Link it again.".into(),
        }
    }
}

/// Shows one OS notification. Clicking it opens the window (CMP-R110).
pub trait Notifier: Send + Sync {
    fn show(&self, text: &str);
}

impl Notifier for Box<dyn Notifier> {
    fn show(&self, text: &str) {
        (**self).show(text);
    }
}

/// Headless mode's notifier: it shows nothing (CMP-R1). Causes are still
/// kept, so a log stays awaiting without asking (CMP-R40).
pub struct Silent;

impl Notifier for Silent {
    fn show(&self, _text: &str) {}
}

/// The one notification service of a process.
pub struct Notifications<N: Notifier> {
    notifier: N,
    raised: Mutex<Raised>,
}

#[derive(Default)]
struct Raised {
    /// The causes raised and not cleared since.
    keys: HashSet<CauseKey>,
    /// While held: the causes raised meanwhile, in order, not shown yet.
    held: Option<Vec<Cause>>,
}

impl<N: Notifier> Notifications<N> {
    pub fn new(notifier: N) -> Self {
        Self {
            notifier,
            raised: Mutex::default(),
        }
    }

    /// Notifies unless this cause was raised and hasn't cleared since; while
    /// held, keeps it for [`Notifications::release`] instead. Returns
    /// whether a notification was shown.
    pub fn raise(&self, cause: &Cause) -> bool {
        let mut raised = self.lock();
        if !raised.keys.insert(cause.key()) {
            return false;
        }
        if let Some(held) = &mut raised.held {
            held.push(cause.clone());
            return false;
        }
        drop(raised);
        self.notifier.show(&cause.text());
        true
    }

    /// Marks a cause as gone, so raising it again notifies again. A held
    /// cause that clears is never shown.
    pub fn clear(&self, key: &CauseKey) {
        let mut raised = self.lock();
        raised.keys.remove(key);
        if let Some(held) = &mut raised.held {
            held.retain(|cause| cause.key() != *key);
        }
    }

    /// Shows no notification from now on until [`Notifications::release`]:
    /// the first-run window is open (CMP-R100).
    pub fn hold(&self) {
        self.lock().held.get_or_insert_with(Vec::new);
    }

    /// Whether notifications are held (CMP-R100).
    pub fn is_held(&self) -> bool {
        self.lock().held.is_some()
    }

    /// Ends the hold and shows every cause raised during it that hasn't
    /// cleared since, each once. Returns how many were shown.
    pub fn release(&self) -> usize {
        let held = self.lock().held.take().unwrap_or_default();
        for cause in &held {
            self.notifier.show(&cause.text());
        }
        held.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Raised> {
        self.raised.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Default, Clone)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Notifier for Recorder {
        fn show(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }
    }

    impl Recorder {
        fn shown(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    fn divergence(log_id: &str) -> Cause {
        Cause::Divergence {
            log_id: log_id.into(),
        }
    }

    #[test]
    fn a_cause_raised_twice_notifies_once() {
        // CMP-R110: each is raised once per cause per process
        let recorder = Recorder::default();
        let notifications = Notifications::new(recorder.clone());

        assert!(notifications.raise(&divergence("3f9a2c1e7b0d4a65")));
        assert!(!notifications.raise(&divergence("3f9a2c1e7b0d4a65")));
        assert_eq!(recorder.shown().len(), 1);
    }

    #[test]
    fn a_cleared_cause_notifies_again() {
        // CMP-R110: again only after that cause has cleared
        let recorder = Recorder::default();
        let notifications = Notifications::new(recorder.clone());

        notifications.raise(&divergence("3f9a2c1e7b0d4a65"));
        notifications.clear(&divergence("3f9a2c1e7b0d4a65").key());
        assert!(notifications.raise(&divergence("3f9a2c1e7b0d4a65")));
        assert_eq!(recorder.shown().len(), 2);
    }

    #[test]
    fn the_same_kind_for_another_log_id_is_another_cause() {
        // CMP-R111: per-logId causes are each their own cause
        let recorder = Recorder::default();
        let notifications = Notifications::new(recorder.clone());

        notifications.raise(&divergence("3f9a2c1e7b0d4a65"));
        assert!(notifications.raise(&divergence("0000000000000001")));
        assert_eq!(recorder.shown().len(), 2);
    }

    #[test]
    fn a_new_message_for_a_raised_cause_does_not_notify_again() {
        // CMP-R110: the cause, not its text, decides
        let recorder = Recorder::default();
        let notifications = Notifications::new(recorder.clone());

        notifications.raise(&Cause::Suspended {
            message: "one".into(),
        });
        assert!(!notifications.raise(&Cause::Suspended {
            message: "two".into()
        }));
        assert_eq!(recorder.shown(), ["one"]);
    }

    #[test]
    fn nothing_is_shown_while_held() {
        // CMP-R100: no notification is raised while the first-run window is
        // open
        let recorder = Recorder::default();
        let notifications = Notifications::new(recorder.clone());
        notifications.hold();
        assert!(notifications.is_held());

        assert!(!notifications.raise(&divergence("3f9a2c1e7b0d4a65")));
        assert!(!notifications.raise(&Cause::CredentialInvalid));
        assert!(recorder.shown().is_empty());
    }

    #[test]
    fn a_held_cause_is_shown_once_at_release_unless_it_cleared() {
        // CMP-R100: none while the first-run window is open; CMP-R110: once
        // per cause, again only after it cleared
        let recorder = Recorder::default();
        let notifications = Notifications::new(recorder.clone());
        notifications.hold();
        notifications.raise(&divergence("3f9a2c1e7b0d4a65"));
        notifications.raise(&divergence("3f9a2c1e7b0d4a65"));
        notifications.raise(&divergence("0000000000000001"));
        notifications.clear(&divergence("0000000000000001").key());

        assert_eq!(notifications.release(), 1);
        assert!(!notifications.is_held());
        assert_eq!(
            recorder.shown(),
            ["This Catch log differs from the copy on Pagle."]
        );
        // Raised already: not again until it clears.
        assert!(!notifications.raise(&divergence("3f9a2c1e7b0d4a65")));
        assert!(notifications.raise(&divergence("0000000000000001")));
        assert_eq!(recorder.shown().len(), 2);
    }

    #[test]
    fn every_cause_has_its_text() {
        // CMP-R111: the table of causes and texts
        let cases = [
            (
                Cause::AwaitingAnswer {
                    log_id: "3f9a2c1e7b0d4a65".into(),
                    characters: "Ada Brook, Kukli Vail".into(),
                },
                "Found a Catch log: Ada Brook, Kukli Vail. Upload it?",
            ),
            (
                Cause::SeveralFolders {
                    log_id: "3f9a2c1e7b0d4a65".into(),
                    folders: 2,
                },
                "The same Catch log is in 2 account folders. Pick the one you play on.",
            ),
            (
                divergence("3f9a2c1e7b0d4a65"),
                "This Catch log differs from the copy on Pagle.",
            ),
            (
                Cause::HeldCharacters {
                    log_id: "3f9a2c1e7b0d4a65".into(),
                },
                "A new character needs verifying. Sign in again to verify it.",
            ),
            (
                Cause::LogCap {
                    message: "You have 20 Catch logs.".into(),
                },
                "You have 20 Catch logs.",
            ),
            (
                Cause::CredentialInvalid,
                "This Companion is no longer linked. Link it again.",
            ),
            (
                Cause::Suspended {
                    message: "Your Pagle account is suspended.".into(),
                },
                "Your Pagle account is suspended.",
            ),
        ];
        for (cause, text) in cases {
            assert_eq!(cause.text(), text);
        }
    }

    #[test]
    fn the_update_required_notification_shows_once_per_run() {
        // DST-R57: shown once per process run with its picked text; nothing
        // clears it, so a later pick or a second entry doesn't notify again
        // (DST-R96)
        let recorder = Recorder::default();
        let notifications = Notifications::new(recorder.clone());
        let download = Cause::UpdateRequired {
            text: "Download the latest version.".into(),
        };
        assert!(notifications.raise(&download));
        assert!(!notifications.raise(&Cause::UpdateRequired {
            text: "Choose Restart to update.".into(),
        }));
        assert_eq!(recorder.shown(), ["Download the latest version."]);
    }

    #[test]
    fn the_nudge_notifies_once_per_run_with_its_text() {
        // DST-R78: once per process run, "An important update for
        // Pagle Companion is ready. Choose Restart to update in its tray
        // menu."; a cause of its own beside DST-R57's
        let recorder = Recorder::default();
        let notifications = Notifications::new(recorder.clone());
        assert!(notifications.raise(&Cause::UpdateRequired {
            text: "Download the latest version.".into(),
        }));
        assert!(notifications.raise(&Cause::UpdateRecommended));
        assert!(!notifications.raise(&Cause::UpdateRecommended));
        assert_eq!(
            recorder.shown(),
            [
                "Download the latest version.",
                "An important update for Pagle Companion is ready. Choose Restart to update in its tray menu."
            ]
        );
    }
}
