//! The Secret Service (Linux; CMP Contracts, Credential store entries): an
//! item in the default collection with attributes `service=pagle-companion`,
//! `kind=credential`, label "Pagle Companion credential".
//!
//! CMP-R62's rules: the Companion never asks the Secret Service to unlock (no
//! `Unlock` call on a read, write or delete); a locked default collection, or
//! a call that returns a prompt (dismissed at once), counts as "doesn't
//! answer"; and every call gives up after the D-Bus timeout. The secret
//! travels over the per-user session bus in the `plain` algorithm.

use std::collections::HashMap;
use std::time::Duration;

use zbus::blocking::{Connection, Proxy, connection, proxy};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::{CredentialStore, StoreError};

/// D-Bus timeout (CMP Tunables): a call without an answer by then counts as
/// "doesn't answer".
pub const D_BUS_TIMEOUT: Duration = Duration::from_secs(5);

const BUS_NAME: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM: &str = "org.freedesktop.Secret.Item";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
const SESSION: &str = "org.freedesktop.Secret.Session";
const LABEL: &str = "Pagle Companion credential";

/// The Secret Service's `Secret` struct, `(oayays)`: session, parameters,
/// value, content type.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

pub struct SecretService {
    service: String,
}

impl SecretService {
    /// The Companion's own item.
    pub fn new() -> Self {
        Self::with_service("pagle-companion")
    }

    /// An item with another `service` attribute: tests use their own, never
    /// the real one.
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn attributes(&self) -> HashMap<&str, &str> {
        HashMap::from([("service", self.service.as_str()), ("kind", "credential")])
    }

    /// Opens a session on the default collection when it is unlocked, or
    /// says why not. Never calls `Unlock`.
    fn open(&self) -> Result<OpenCollection, StoreError> {
        let conn = connection::Builder::session()
            .and_then(|b| b.method_timeout(D_BUS_TIMEOUT).build())
            .map_err(unavailable)?;
        let service_path =
            OwnedObjectPath::try_from(SERVICE_PATH).map_err(|e| unavailable(e.into()))?;
        let service = proxy(&conn, &service_path, SERVICE)?;
        let (_, session): (OwnedValue, OwnedObjectPath) = service
            .call("OpenSession", &("plain", Value::from("")))
            .map_err(unavailable)?;
        let open = Open {
            conn: conn.clone(),
            session,
        };
        let collection: OwnedObjectPath = service
            .call("ReadAlias", &("default",))
            .map_err(unavailable)?;
        if collection.as_str() == "/" {
            return Err(StoreError::Unavailable("no default collection".into()));
        }
        let locked: bool = open
            .proxy(&collection, COLLECTION)?
            .get_property("Locked")
            .map_err(unavailable)?;
        if locked {
            return Err(StoreError::Unavailable(
                "the default collection is locked".into(),
            ));
        }
        Ok(OpenCollection { open, collection })
    }
}

impl Default for SecretService {
    fn default() -> Self {
        Self::new()
    }
}

/// A session with the Secret Service, closed when dropped.
struct Open {
    conn: Connection,
    session: OwnedObjectPath,
}

impl Open {
    fn proxy<'p>(
        &self,
        path: &'p OwnedObjectPath,
        interface: &'static str,
    ) -> Result<Proxy<'p>, StoreError> {
        proxy(&self.conn, path, interface)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let session = self.session.clone();
        if let Ok(proxy) = self.proxy(&session, SESSION) {
            let _: Result<(), _> = proxy.call("Close", &());
        }
    }
}

/// A session on the unlocked default collection.
struct OpenCollection {
    open: Open,
    collection: OwnedObjectPath,
}

impl OpenCollection {
    fn items(&self, attributes: &HashMap<&str, &str>) -> Result<Vec<OwnedObjectPath>, StoreError> {
        self.open
            .proxy(&self.collection, COLLECTION)?
            .call("SearchItems", &(attributes,))
            .map_err(unavailable)
    }

    /// A call that returned a prompt asks something of the Player: dismiss
    /// it and count the call as unanswered (CMP-R62).
    fn refuse_prompt(&self, prompt: &OwnedObjectPath) -> Result<(), StoreError> {
        if prompt.as_str() == "/" {
            return Ok(());
        }
        if let Ok(proxy) = self.open.proxy(prompt, PROMPT) {
            let _: Result<(), _> = proxy.call("Dismiss", &());
        }
        Err(StoreError::Unavailable(
            "the Secret Service asked for a prompt".into(),
        ))
    }
}

impl CredentialStore for SecretService {
    fn place(&self) -> &'static str {
        "your keyring"
    }

    fn read(&self) -> Result<Option<String>, StoreError> {
        let open = self.open()?;
        let Some(item) = open.items(&self.attributes())?.into_iter().next() else {
            return Ok(None);
        };
        let (_, _, value, _): Secret = open
            .open
            .proxy(&item, ITEM)?
            .call("GetSecret", &(&open.open.session,))
            .map_err(unavailable)?;
        let credential = String::from_utf8(value)
            .map_err(|_| StoreError::Failed("the stored credential isn't UTF-8".into()))?;
        let credential = credential.trim_end_matches(['\n', '\r']).to_owned();
        Ok((!credential.is_empty()).then_some(credential))
    }

    fn write(&self, credential: &str) -> Result<(), StoreError> {
        let open = self.open()?;
        let properties: HashMap<&str, Value<'_>> = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", Value::from(LABEL)),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(self.attributes()),
            ),
        ]);
        let secret: Secret = (
            open.open.session.clone(),
            Vec::new(),
            credential.as_bytes().to_vec(),
            "text/plain".to_owned(),
        );
        let (_, prompt): (OwnedObjectPath, OwnedObjectPath) = open
            .open
            .proxy(&open.collection, COLLECTION)?
            .call("CreateItem", &(properties, secret, true))
            .map_err(unavailable)?;
        open.refuse_prompt(&prompt)
    }

    fn delete(&self) -> Result<(), StoreError> {
        let open = self.open()?;
        for item in open.items(&self.attributes())? {
            let prompt: OwnedObjectPath = open
                .open
                .proxy(&item, ITEM)?
                .call("Delete", &())
                .map_err(unavailable)?;
            open.refuse_prompt(&prompt)?;
        }
        Ok(())
    }
}

/// A proxy that doesn't cache properties, so it makes no calls of its own.
fn proxy<'p>(
    conn: &Connection,
    path: &'p OwnedObjectPath,
    interface: &'static str,
) -> Result<Proxy<'p>, StoreError> {
    proxy::Builder::<Proxy<'p>>::new(conn)
        .destination(BUS_NAME)
        .and_then(|b| b.path(path.as_ref()))
        .and_then(|b| b.interface(interface))
        .map(|b| b.cache_properties(CacheProperties::No))
        .and_then(|b| b.build())
        .map_err(unavailable)
}

/// Every D-Bus failure counts as "doesn't answer" (CMP-R62).
fn unavailable(e: zbus::Error) -> StoreError {
    StoreError::Unavailable(format!("the Secret Service doesn't answer: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_d_bus_timeout_is_five_seconds() {
        // CMP Tunables: D-Bus timeout (Secret Service), 5 s
        assert_eq!(D_BUS_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn the_item_has_the_contracts_attributes() {
        // CMP Contracts, Credential store entries: service=pagle-companion,
        // kind=credential, label "Pagle Companion credential"
        let store = SecretService::new();
        let attributes = store.attributes();
        assert_eq!(attributes["service"], "pagle-companion");
        assert_eq!(attributes["kind"], "credential");
        assert_eq!(LABEL, "Pagle Companion credential");
        assert_eq!(store.place(), "your keyring");
    }

    #[test]
    fn an_unlocked_secret_service_keeps_reads_back_and_deletes_the_credential() {
        // CMP-R62: the Secret Service when its default collection is already
        // unlocked. Runners without an unlocked keyring skip this.
        let store =
            SecretService::with_service(format!("pagle-companion-test-{}", std::process::id()));
        match store.write("pgc_first") {
            Ok(()) => {}
            Err(StoreError::Unavailable(why)) => {
                eprintln!("skipped: no unlocked Secret Service ({why})");
                return;
            }
            Err(e) => panic!("{e}"),
        }
        let kept = store.read();
        let rotated = store.write("pgc_successor").and_then(|()| store.read());
        let deleted = store.delete().and_then(|()| store.read());
        assert_eq!(kept.unwrap().as_deref(), Some("pgc_first"));
        assert_eq!(rotated.unwrap().as_deref(), Some("pgc_successor"));
        assert_eq!(deleted.unwrap(), None);
    }
}
