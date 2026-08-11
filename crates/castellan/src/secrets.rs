//! In-memory secret store (G7): agent secret bundles live only in daemon
//! RAM. Nothing plaintext is written to disk — bundles arrive via create /
//! restore / pull and are re-pulled from suzerain after a daemon restart.

use std::collections::HashMap;
use std::sync::RwLock;

use suzerain_protocol::secrets::SecretBundle;
use uuid::Uuid;

static STORE: RwLock<Option<HashMap<Uuid, SecretBundle>>> = RwLock::new(None);

fn with_store<R>(f: impl FnOnce(&mut HashMap<Uuid, SecretBundle>) -> R) -> R {
    let mut guard = STORE.write().unwrap();
    let store = guard.get_or_insert_with(HashMap::new);
    f(store)
}

/// Store a bundle; register all its values for journal redaction.
pub fn put(id: Uuid, bundle: SecretBundle) {
    for value in bundle.values() {
        crate::journal::register_secret(value);
    }
    with_store(|s| {
        s.insert(id, bundle);
    });
}

pub fn get(id: &Uuid) -> Option<SecretBundle> {
    with_store(|s| s.get(id).cloned())
}

pub fn remove(id: &Uuid) {
    with_store(|s| {
        s.remove(id);
    });
}
