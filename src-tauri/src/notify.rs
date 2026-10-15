//! Notifications (CMP-R110, CMP-R111): raised in tray mode only when the
//! Player must act, each once per cause per process, and again only after
//! that cause has cleared. A successful Upload never notifies.

use std::collections::HashSet;
use std::sync::Mutex;

/// Why the Companion needs the Player (CMP-R111). The update-required
/// notification is `companion/distribution.md`'s (DST-R57) and comes with
/// Track C.
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
}

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
            Cause::CredentialInvalid => "This Companion is no longer linked. Link it again.".into(),
        }
    }
}

/// Shows one OS notification. Clicking it opens the window (CMP-R110).
pub trait Notifier: Send + Sync {
    fn show(&self, text: &str);
}

/// The one notification service of a tray-mode process.
pub struct Notifications<N: Notifier> {
    notifier: N,
    raised: Mutex<HashSet<CauseKey>>,
}

impl<N: Notifier> Notifications<N> {
    pub fn new(notifier: N) -> Self {
        Self {
            notifier,
            raised: Mutex::new(HashSet::new()),
        }
    }

    /// Notifies unless this cause was raised and hasn't cleared since.
    /// Returns whether a notification was shown.
    pub fn raise(&self, cause: &Cause) -> bool {
        let fresh = self.lock().insert(cause.key());
        if fresh {
            self.notifier.show(&cause.text());
        }
        fresh
    }

    /// Marks a cause as gone, so raising it again notifies again.
    pub fn clear(&self, key: &CauseKey) {
        self.lock().remove(key);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<CauseKey>> {
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
}
