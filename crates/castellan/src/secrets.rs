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

/// Store a bundle; register its values for journal redaction (replacing
/// whatever was registered for this agent before — see
/// `journal::register_secrets`).
pub fn put(id: Uuid, bundle: SecretBundle) {
    crate::journal::register_secrets(id, bundle.values().map(str::to_string));
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
    // Otherwise this agent's secrets sit in the redaction list forever —
    // the daemon-lifetime growth this is here to prevent.
    crate::journal::unregister_secrets(id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use suzerain_protocol::secrets::SecretEntry;

    fn bundle_with(value: &str) -> SecretBundle {
        let mut bundle = SecretBundle::default();
        bundle.env.insert(
            "API_KEY".to_string(),
            SecretEntry {
                value: value.to_string(),
                hosts: vec!["api.example.com".to_string()],
            },
        );
        bundle.git_ssh_key = Some("-----BEGIN OPENSSH PRIVATE KEY-----fake".to_string());
        bundle
    }

    #[test]
    fn put_then_get_round_trips_the_bundle() {
        let id = Uuid::new_v4();
        let bundle = bundle_with("super-secret-value-123");
        put(id, bundle.clone());

        let fetched = get(&id).expect("bundle should be present after put");
        assert_eq!(fetched.env["API_KEY"].value, bundle.env["API_KEY"].value);
        assert_eq!(fetched.git_ssh_key, bundle.git_ssh_key);

        remove(&id);
    }

    #[test]
    fn get_of_unknown_id_returns_none() {
        let id = Uuid::new_v4();
        assert!(get(&id).is_none());
    }

    #[test]
    fn put_replaces_rather_than_merges_the_previous_bundle() {
        let id = Uuid::new_v4();
        put(id, bundle_with("first-secret-value-here"));
        put(id, bundle_with("second-secret-value-here"));

        let fetched = get(&id).unwrap();
        assert_eq!(fetched.env["API_KEY"].value, "second-secret-value-here");
        assert_eq!(fetched.env.len(), 1, "no stale keys from the old bundle");

        remove(&id);
    }

    #[test]
    fn remove_drops_the_bundle_and_unregisters_journal_redaction() {
        let id = Uuid::new_v4();
        let secret_value = "very-secret-remove-test-value";
        put(id, bundle_with(secret_value));

        // put() must have registered the value for journal redaction.
        assert_eq!(
            crate::journal::redact(id, &format!("token={secret_value}")),
            "token=[REDACTED]"
        );

        remove(&id);

        assert!(get(&id).is_none());
        // remove() must also purge the redaction registration — otherwise
        // it (harmlessly, since the value is gone, but wastefully) grows
        // the daemon-lifetime redaction list forever.
        assert_eq!(
            crate::journal::redact(id, &format!("token={secret_value}")),
            format!("token={secret_value}")
        );
    }

    #[test]
    fn is_empty_reflects_bundle_contents() {
        assert!(SecretBundle::default().is_empty());
        assert!(!bundle_with("anything").is_empty());
    }

    #[test]
    fn values_yields_every_env_value_and_the_ssh_key() {
        let bundle = bundle_with("env-secret-value-1");
        let values: Vec<&str> = bundle.values().collect();
        assert_eq!(values.len(), 2);
        assert!(values.contains(&"env-secret-value-1"));
        assert!(values.contains(&"-----BEGIN OPENSSH PRIVATE KEY-----fake"));
    }
}
