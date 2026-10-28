use std::cell::{Cell, RefCell};
use std::path::Path;

use tempfile::TempDir;

use super::{
    Credential, CredentialStorage, CredentialWriteError, ISSUER, Registration,
    RegistrationPersistence, RegistrationUpdate, Registry, registration_id,
};
use crate::error::SpecLoopError;

fn credential(token: &str) -> Credential {
    Credential {
        access_token: token.to_owned(),
        refresh_token: None,
        id_token: "fixture-id".to_owned(),
        scopes: vec!["fixture".to_owned()],
        expires_at_unix: Some(1_900_000_000),
    }
}

fn identity() -> String {
    registration_id(ISSUER, "fixture-subject", "fixture-client").expect("identity")
}

fn registration(storage: CredentialStorage) -> Registration {
    Registration {
        id: identity(),
        issuer: ISSUER.to_owned(),
        subject: "fixture-subject".to_owned(),
        client_id: "fixture-client".to_owned(),
        credential_storage: storage,
        fallback_credential: matches!(storage, CredentialStorage::File).then(|| credential("old")),
    }
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    None,
    Staging,
    Credential,
    Activation,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    previous: Option<CredentialStorage>,
    fallback: bool,
    failure: Failure,
    credential_restore_fails: bool,
    metadata_restore_fails: bool,
}

fn exercise(case: Case) {
    let temporary = TempDir::new().expect("auth fixture");
    let path = temporary.path().join("auth.json");
    let mut registry = Registry::empty();
    if let Some(storage) = case.previous {
        registry.registrations.push(registration(storage));
        registry.active_registration_id = Some(identity());
    }
    registry.save(&path).expect("original metadata");
    let original = serde_json::to_value(&registry).expect("original registry");
    let store = RefCell::new(
        case.previous
            .filter(|storage| matches!(storage, CredentialStorage::Keyring))
            .map(|_| credential("old")),
    );
    let operations = RefCell::new(Vec::new());
    let saves = Cell::new(0_u32);
    let update = RegistrationUpdate {
        registration: registration(CredentialStorage::Keyring),
        credential: credential("new"),
        selected_id: case.previous.map(|_| identity()),
    };
    let result = RegistrationPersistence {
        save_registry: |candidate: &Registry, path: &Path| {
            let number = saves.get().checked_add(1_u32).expect("save count");
            saves.set(number);
            operations.borrow_mut().push(match (number, case.failure) {
                (1, _) => "stage",
                (2, Failure::Credential) => "restore metadata",
                (2, _) => "activate",
                _ => "restore metadata",
            });
            if (number == 1 && matches!(case.failure, Failure::Staging))
                || (number == 2 && matches!(case.failure, Failure::Activation))
            {
                return Err(SpecLoopError::Workflow("original save failure".to_owned()));
            }
            if number >= 3 && case.metadata_restore_fails {
                return Err(SpecLoopError::Workflow(
                    "metadata restore sentinel".to_owned(),
                ));
            }
            candidate.save(path)
        },
        read_credential: |_: &Registration| {
            operations.borrow_mut().push("read");
            Ok(store.borrow().clone())
        },
        write_credential: |_: &str, value: &Credential| {
            operations.borrow_mut().push("write");
            if case.fallback {
                return Err(CredentialWriteError::Unavailable);
            }
            *store.borrow_mut() = Some(value.clone());
            if matches!(case.failure, Failure::Credential) {
                return Err(CredentialWriteError::Failed(SpecLoopError::Workflow(
                    "original credential failure".to_owned(),
                )));
            }
            Ok(())
        },
        restore_credential: |_: &str, previous: Option<&Credential>| {
            operations.borrow_mut().push("restore credential");
            if case.credential_restore_fails {
                return Err(SpecLoopError::Workflow(
                    "credential restore sentinel".to_owned(),
                ));
            }
            *store.borrow_mut() = previous.cloned();
            Ok(())
        },
    }
    .persist(&mut registry, &path, update);
    assert_eq!(*operations.borrow(), expected_operations(case), "{case:?}");
    verify_persistence(case, result, &registry, &original, &store, &path);
}

fn expected_operations(case: Case) -> Vec<&'static str> {
    let mut expected = Vec::new();
    if matches!(case.previous, Some(CredentialStorage::Keyring)) {
        expected.push("read");
    }
    expected.push("stage");
    if !matches!(case.failure, Failure::Staging) {
        expected.push("write");
        if !matches!(case.failure, Failure::Credential) {
            expected.push("activate");
        }
        if !matches!(case.failure, Failure::None) {
            if !case.fallback {
                expected.push("restore credential");
            }
            if case.previous.is_none() {
                expected.push("restore metadata");
            }
        }
    }
    expected
}

fn verify_persistence(
    case: Case,
    result: crate::error::Result<()>,
    registry: &Registry,
    original: &serde_json::Value,
    store: &RefCell<Option<Credential>>,
    path: &Path,
) {
    let persisted = Registry::load(path).expect("persisted metadata");
    if matches!(case.failure, Failure::None) {
        result.expect("activation succeeds");
        assert_eq!(
            persisted.active_registration_id.as_deref(),
            Some(identity().as_str())
        );
        assert_eq!(
            serde_json::to_value(registry).expect("in-memory"),
            serde_json::to_value(&persisted).expect("on disk")
        );
        if case.fallback {
            assert_eq!(
                persisted
                    .registrations
                    .first()
                    .and_then(|entry| entry.fallback_credential.as_ref())
                    .map(|value| value.access_token.as_str()),
                Some("new")
            );
        } else {
            assert_eq!(
                store
                    .borrow()
                    .as_ref()
                    .map(|value| value.access_token.as_str()),
                Some("new")
            );
        }
    } else {
        let error = result.expect_err("injected failure").to_string();
        assert!(error.contains("original"), "{error}");
        assert_eq!(
            &serde_json::to_value(registry).expect("in-memory"),
            original,
            "failure never activates memory"
        );
        if !case.metadata_restore_fails || case.previous.is_some() {
            assert_eq!(
                &serde_json::to_value(&persisted).expect("metadata restored"),
                original
            );
        }
        if !case.credential_restore_fails {
            assert_eq!(
                store
                    .borrow()
                    .as_ref()
                    .map(|value| value.access_token.as_str()),
                case.previous
                    .filter(|storage| matches!(storage, CredentialStorage::Keyring))
                    .map(|_| "old")
            );
        }
        if case.credential_restore_fails {
            assert!(error.contains("credential restore sentinel"));
        }
        if case.metadata_restore_fails {
            assert!(error.contains("metadata restore sentinel"));
        }
        match (case.credential_restore_fails, case.metadata_restore_fails) {
            (true, true) => assert!(
                error.contains("credential rollback failed")
                    && error.contains("registry rollback failed")
            ),
            (true, false) => assert!(error.contains("restoring prior credential also failed")),
            (false, true) => {
                assert!(error.contains("restoring prior registry metadata also failed"));
            }
            (false, false) => {}
        }
    }
}

#[test]
fn persistence_success_and_failure_order_preserves_prior_credentials_and_metadata() {
    for previous in [
        None,
        Some(CredentialStorage::Keyring),
        Some(CredentialStorage::File),
    ] {
        for fallback in [false, true] {
            for failure in [
                Failure::None,
                Failure::Staging,
                Failure::Credential,
                Failure::Activation,
            ] {
                if fallback && matches!(failure, Failure::Credential) {
                    continue;
                }
                exercise(Case {
                    previous,
                    fallback,
                    failure,
                    credential_restore_fails: false,
                    metadata_restore_fails: false,
                });
            }
        }
    }
}

#[test]
fn restoration_failures_retain_original_and_all_failed_restoration_diagnostics() {
    for (credential_restore_fails, metadata_restore_fails) in
        [(true, false), (false, true), (true, true)]
    {
        exercise(Case {
            previous: None,
            fallback: false,
            failure: Failure::Activation,
            credential_restore_fails,
            metadata_restore_fails,
        });
    }
}

#[test]
fn identity_duplicate_and_prior_read_failures_never_write() {
    for mode in ["identity", "duplicate", "read"] {
        let temporary = TempDir::new().expect("auth fixture");
        let path = temporary.path().join("auth.json");
        let mut registry = Registry::empty();
        registry
            .registrations
            .push(registration(CredentialStorage::Keyring));
        let original = serde_json::to_value(&registry).expect("original");
        let selected_id = match mode {
            "identity" => Some("different".to_owned()),
            "duplicate" => None,
            _ => Some(identity()),
        };
        let result = RegistrationPersistence {
            save_registry: |_: &Registry, _: &Path| panic!("unexpected write"),
            write_credential: |_: &str, _: &Credential| panic!("unexpected credential write"),
            read_credential: |_: &Registration| {
                Err(SpecLoopError::Workflow("read failed".to_owned()))
            },
            restore_credential: |_: &str, _: Option<&Credential>| panic!("unexpected restoration"),
        }
        .persist(
            &mut registry,
            &path,
            RegistrationUpdate {
                registration: registration(CredentialStorage::Keyring),
                credential: credential("new"),
                selected_id,
            },
        );
        assert!(result.is_err());
        assert_eq!(
            serde_json::to_value(&registry).expect("unchanged"),
            original
        );
        assert!(!path.exists());
    }
}
