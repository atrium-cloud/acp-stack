use super::*;
use tempfile::TempDir;

fn fresh_home() -> TempDir {
    TempDir::new().expect("tempdir")
}

#[test]
fn open_or_create_initializes_empty_store() {
    let home = fresh_home();
    let store = SecretStore::open_or_create(home.path()).expect("open or create");
    assert!(store.list_names().is_empty());
    assert!(age_key_path(home.path()).exists());
    assert!(secret_store_path(home.path()).exists());
}

#[test]
fn set_get_delete_roundtrip() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    store.set("FOO", "bar").expect("set");
    assert_eq!(store.get("FOO").expect("get"), "bar");
    assert!(store.contains("FOO"));
    store.delete("FOO").expect("delete");
    assert!(matches!(
        store.get("FOO"),
        Err(StackError::SecretNotFound { .. })
    ));
}

#[test]
fn reopen_preserves_secrets() {
    let home = fresh_home();
    {
        let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
        store.set("ALPHA", "1").expect("set alpha");
        store.set("BETA", "2").expect("set beta");
    }
    let store = SecretStore::open(home.path()).expect("reopen");
    assert_eq!(store.get("ALPHA").unwrap(), "1");
    assert_eq!(store.get("BETA").unwrap(), "2");
    let names = store.list_names();
    assert_eq!(names, vec!["ALPHA", "BETA"]);
}

#[test]
fn legacy_plaintext_defaults_provider_catalog_to_empty() {
    let plaintext: StorePlaintext =
        toml::from_str("[secrets]\nALPHA = \"1\"\n").expect("legacy plaintext");

    assert_eq!(
        plaintext.secrets.get("ALPHA").map(String::as_str),
        Some("1")
    );
    assert!(plaintext.provider_credentials.is_empty());
}

#[test]
fn provider_credentials_round_trip_without_exposing_values_in_debug() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    let credential = ProviderCredential::new(
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "private-value".to_owned())]),
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "SOURCE_KEY".to_owned())]),
    );
    let revision = credential.revision.clone();
    store
        .replace_provider_credentials(
            BTreeMap::from([(
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(credential),
            )]),
            &[],
        )
        .expect("persist catalog");

    let reopened = SecretStore::open(home.path()).expect("reopen");
    let credential = reopened
        .provider_credential_set("opencode-go")
        .and_then(|set| set.sole.as_ref())
        .expect("credential");
    assert_eq!(credential.revision, revision);
    assert_eq!(credential.values["OPENCODE_API_KEY"], "private-value");
    let debug = format!("{credential:?}");
    assert!(!debug.contains("private-value"));
    assert!(!debug.contains(&revision));
}

#[test]
fn staged_provider_credentials_are_not_persisted_until_replaced() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    let persisted = ProviderCredential::new(
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "persisted".to_owned())]),
        BTreeMap::new(),
    );
    store
        .replace_provider_credentials(
            BTreeMap::from([(
                "opencode-go".to_owned(),
                ProviderCredentialSet::aliasless(persisted),
            )]),
            &[],
        )
        .expect("persist catalog");

    let staged = ProviderCredential::new(
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "staged".to_owned())]),
        BTreeMap::new(),
    );
    store
        .stage_provider_credentials(BTreeMap::from([(
            "opencode-go".to_owned(),
            ProviderCredentialSet::aliasless(staged),
        )]))
        .expect("stage catalog");
    assert_eq!(
        store
            .provider_credential_set("opencode-go")
            .and_then(|set| set.sole.as_ref())
            .expect("staged credential")
            .values["OPENCODE_API_KEY"],
        "staged"
    );

    let reopened = SecretStore::open(home.path()).expect("reopen");
    assert_eq!(
        reopened
            .provider_credential_set("opencode-go")
            .and_then(|set| set.sole.as_ref())
            .expect("persisted credential")
            .values["OPENCODE_API_KEY"],
        "persisted"
    );
}

#[test]
fn rotating_provider_credential_changes_revision_and_keeps_alias_mode() {
    let mut credential = ProviderCredential::new(
        BTreeMap::from([("OPENROUTER_API_KEY".to_owned(), "first".to_owned())]),
        BTreeMap::new(),
    );
    let previous_revision = credential.revision.clone();
    credential.rotate(
        BTreeMap::from([("OPENROUTER_API_KEY".to_owned(), "second".to_owned())]),
        BTreeMap::new(),
    );
    let set = ProviderCredentialSet::promoted(BTreeMap::from([("backup".to_owned(), credential)]));

    assert!(set.is_promoted());
    let selected = set.selected(Some("backup")).expect("selected alias").0;
    assert_ne!(selected.revision, previous_revision);
    assert_eq!(selected.values["OPENROUTER_API_KEY"], "second");
}

#[test]
fn delete_unknown_secret_errors() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    let error = store.delete("NOT_THERE").expect_err("must error");
    assert!(matches!(error, StackError::SecretNotFound { .. }));
}

#[test]
fn open_without_init_fails() {
    let home = fresh_home();
    let error = SecretStore::open(home.path()).expect_err("must fail");
    assert!(matches!(error, StackError::AgeKeyRead { .. }));
}

#[test]
fn open_with_corrupt_age_key_errors() {
    let home = fresh_home();
    let key_path = age_key_path(home.path());
    std::fs::create_dir_all(key_path.parent().unwrap()).unwrap();
    std::fs::write(&key_path, "not-an-age-key").unwrap();
    let error = SecretStore::open(home.path()).expect_err("must fail");
    assert!(matches!(error, StackError::AgeKeyParse { .. }));
}

fn selection(provider_id: &str, env_name: &str, value: &str) -> ManagedCredentialSelection {
    ManagedCredentialSelection {
        provider_id: provider_id.to_owned(),
        values: BTreeMap::from([(env_name.to_owned(), value.to_owned())]),
        source_refs: BTreeMap::new(),
    }
}

#[test]
fn legacy_plaintext_defaults_source_to_operator_and_managed_state_to_empty() {
    let plaintext: StorePlaintext = toml::from_str(
        "[secrets]\nALPHA = \"1\"\n\
         [provider_credentials.openai.sole]\n\
         revision = \"r1\"\n\
         [provider_credentials.openai.sole.values]\n\
         OPENAI_API_KEY = \"sk\"\n",
    )
    .expect("legacy plaintext");
    let credential = plaintext.provider_credentials["openai"]
        .sole
        .as_ref()
        .expect("sole credential");
    assert_eq!(credential.source, CredentialSource::Operator);
    assert!(plaintext.managed_state.is_empty());
}

#[test]
fn operator_entries_serialize_without_source_field() {
    let plaintext = StorePlaintext {
        secrets: BTreeMap::new(),
        provider_credentials: BTreeMap::from([(
            "openai".to_owned(),
            ProviderCredentialSet::aliasless(ProviderCredential::new(
                BTreeMap::from([("OPENAI_API_KEY".to_owned(), "sk".to_owned())]),
                BTreeMap::new(),
            )),
        )]),
        managed_state: BTreeMap::new(),
    };
    let serialized = toml::to_string(&plaintext).expect("serialize");
    assert!(
        !serialized.contains("source"),
        "operator entries must stay byte-identical on disk, got:\n{serialized}"
    );
}

#[test]
fn managed_apply_persists_credential_and_watermark_atomically() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    let outcome = store
        .apply_managed_state_credential(
            "platform-state",
            "provider-credential",
            7,
            Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
        )
        .expect("apply");
    assert_eq!(outcome, ManagedApplyOutcome::Applied);

    let reopened = SecretStore::open(home.path()).expect("reopen");
    let record = reopened
        .managed_state_record("platform-state")
        .expect("watermark record");
    assert_eq!(record.revision, 7);
    assert_eq!(record.provider_id.as_deref(), Some("openai"));
    assert_eq!(record.kind.as_deref(), Some("provider-credential"));
    let credential = reopened
        .provider_credential_set("openai")
        .and_then(|set| set.sole.as_ref())
        .expect("stored credential");
    assert_eq!(credential.values["OPENAI_API_KEY"], "sk-managed");
    assert_eq!(
        credential.source,
        CredentialSource::External("platform-state".to_owned())
    );
    let debug = format!("{credential:?}");
    assert!(!debug.contains("sk-managed"));
}

#[test]
fn managed_apply_replay_is_noop_and_divergent_replay_conflicts() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    store
        .apply_managed_state_credential(
            "platform-state",
            "provider-credential",
            7,
            Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
        )
        .expect("apply");

    let replay = store
        .apply_managed_state_credential(
            "platform-state",
            "provider-credential",
            7,
            Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
        )
        .expect("identical replay");
    assert_eq!(replay, ManagedApplyOutcome::Noop);

    let divergent = store
        .apply_managed_state_credential(
            "platform-state",
            "provider-credential",
            7,
            Some(selection("openai", "OPENAI_API_KEY", "sk-other")),
        )
        .expect_err("divergent replay must conflict");
    assert!(matches!(
        divergent,
        StackError::ExtensionRevisionConflict { .. }
    ));

    let stale = store
        .apply_managed_state_credential("platform-state", "provider-credential", 6, None)
        .expect_err("stale revision must conflict");
    assert!(matches!(
        stale,
        StackError::ExtensionRevisionConflict { .. }
    ));
}

#[test]
fn managed_clear_removes_credential_but_retains_watermark() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    store
        .apply_managed_state_credential(
            "platform-state",
            "provider-credential",
            7,
            Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
        )
        .expect("apply");
    let outcome = store
        .apply_managed_state_credential("platform-state", "provider-credential", 8, None)
        .expect("clear");
    assert_eq!(outcome, ManagedApplyOutcome::Cleared);

    let reopened = SecretStore::open(home.path()).expect("reopen");
    assert!(reopened.provider_credential_set("openai").is_none());
    let record = reopened
        .managed_state_record("platform-state")
        .expect("watermark survives clear");
    assert_eq!(record.revision, 8);
    assert!(record.provider_id.is_none());
}

#[test]
fn managed_apply_refuses_operator_and_foreign_namespace_entries() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    store
        .replace_provider_credentials(
            BTreeMap::from([(
                "openai".to_owned(),
                ProviderCredentialSet::aliasless(ProviderCredential::new(
                    BTreeMap::from([("OPENAI_API_KEY".to_owned(), "operator".to_owned())]),
                    BTreeMap::new(),
                )),
            )]),
            &[],
        )
        .expect("seed operator credential");

    let operator_owned = store
        .apply_managed_state_credential(
            "platform-state",
            "provider-credential",
            7,
            Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
        )
        .expect_err("operator entry must be protected");
    assert!(matches!(
        operator_owned,
        StackError::ExtensionStateOwnership { .. }
    ));

    store
        .apply_managed_state_credential(
            "namespace-a",
            "provider-credential",
            1,
            Some(selection("groq", "GROQ_API_KEY", "gk-managed")),
        )
        .expect("namespace-a takes groq");
    let foreign = store
        .apply_managed_state_credential(
            "namespace-b",
            "provider-credential",
            1,
            Some(selection("groq", "GROQ_API_KEY", "gk-other")),
        )
        .expect_err("foreign namespace entry must be protected");
    assert!(matches!(
        foreign,
        StackError::ExtensionStateOwnership { .. }
    ));
}

#[test]
fn operator_replace_refuses_to_clobber_external_entries() {
    let home = fresh_home();
    let mut store = SecretStore::open_or_create(home.path()).expect("open or create");
    store
        .apply_managed_state_credential(
            "platform-state",
            "provider-credential",
            7,
            Some(selection("openai", "OPENAI_API_KEY", "sk-managed")),
        )
        .expect("apply");

    let clobber = store
        .replace_provider_credentials(BTreeMap::new(), &[])
        .expect_err("operator replace must not drop an external entry");
    assert!(matches!(
        clobber,
        StackError::ExtensionStateOwnership { .. }
    ));

    // An operator replace that carries the external entry through
    // unchanged is fine.
    let carried = store.provider_credentials().clone();
    store
        .replace_provider_credentials(carried, &[])
        .expect("carry-through replace succeeds");
}
