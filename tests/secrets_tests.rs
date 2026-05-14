use acp_stack::error::StackError;
use acp_stack::secrets::{SecretStore, age_key_path, secret_store_path};
use std::fs;

#[test]
fn open_or_create_writes_age_key_and_ciphertext_under_home() {
    let home = tempfile::tempdir().expect("tempdir");
    let store = SecretStore::open_or_create(home.path()).expect("init store");
    assert!(store.list_names().is_empty());
    assert!(age_key_path(home.path()).exists());
    assert!(secret_store_path(home.path()).exists());
}

#[cfg(unix)]
#[test]
fn age_key_and_store_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = tempfile::tempdir().expect("tempdir");
    SecretStore::open_or_create(home.path()).expect("init store");

    let key_mode = fs::metadata(age_key_path(home.path()))
        .expect("age key metadata")
        .permissions()
        .mode()
        & 0o777;
    let store_mode = fs::metadata(secret_store_path(home.path()))
        .expect("store metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(key_mode, 0o600, "age key must be 0600, got {key_mode:o}");
    assert_eq!(store_mode, 0o600, "store must be 0600, got {store_mode:o}");
}

#[test]
fn set_and_get_secret_roundtrip() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut store = SecretStore::open_or_create(home.path()).expect("init");
    store.set("OPENCODE_API_KEY", "value-1").expect("set");
    assert_eq!(store.get("OPENCODE_API_KEY").expect("get"), "value-1");
    assert!(store.contains("OPENCODE_API_KEY"));
}

#[test]
fn reopening_recovers_persisted_secrets() {
    let home = tempfile::tempdir().expect("tempdir");
    {
        let mut store = SecretStore::open_or_create(home.path()).expect("init");
        store.set("ALPHA", "1").expect("set alpha");
        store.set("BETA", "2").expect("set beta");
    }

    let reopened = SecretStore::open(home.path()).expect("reopen");
    assert_eq!(reopened.get("ALPHA").unwrap(), "1");
    assert_eq!(reopened.get("BETA").unwrap(), "2");
    let names = reopened.list_names();
    assert_eq!(names, vec!["ALPHA", "BETA"], "names sorted ascending");
}

#[test]
fn delete_removes_secret_and_persists() {
    let home = tempfile::tempdir().expect("tempdir");
    {
        let mut store = SecretStore::open_or_create(home.path()).expect("init");
        store.set("ONE", "x").expect("set one");
        store.set("TWO", "y").expect("set two");
        store.delete("ONE").expect("delete one");
    }

    let reopened = SecretStore::open(home.path()).expect("reopen");
    assert!(!reopened.contains("ONE"));
    assert_eq!(reopened.get("TWO").unwrap(), "y");
}

#[test]
fn delete_missing_secret_errors() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut store = SecretStore::open_or_create(home.path()).expect("init");
    let error = store.delete("NEVER_SET").expect_err("must error");
    assert!(matches!(error, StackError::SecretNotFound { name } if name == "NEVER_SET"));
}

#[test]
fn open_without_init_fails_with_age_key_read_error() {
    let home = tempfile::tempdir().expect("tempdir");
    let error = SecretStore::open(home.path()).expect_err("must fail");
    assert!(matches!(error, StackError::AgeKeyRead { .. }));
}

#[test]
fn corrupt_age_key_is_surfaced_as_parse_error() {
    let home = tempfile::tempdir().expect("tempdir");
    let path = age_key_path(home.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "definitely-not-an-age-key").unwrap();
    let error = SecretStore::open(home.path()).expect_err("must fail");
    assert!(matches!(error, StackError::AgeKeyParse { .. }));
}

#[test]
fn corrupt_ciphertext_is_surfaced_as_decrypt_error() {
    let home = tempfile::tempdir().expect("tempdir");
    {
        let _ = SecretStore::open_or_create(home.path()).expect("init");
    }
    let store_path = secret_store_path(home.path());
    fs::write(&store_path, b"obviously not age ciphertext").unwrap();
    let error = SecretStore::open(home.path()).expect_err("must fail");
    assert!(matches!(error, StackError::SecretStoreDecrypt(_)));
}

#[test]
fn open_or_create_rejects_age_key_without_store() {
    let home = tempfile::tempdir().expect("tempdir");
    // Write only the age key, leaving secrets.age missing.
    {
        let mut store = SecretStore::open_or_create(home.path()).expect("init");
        store.set("KEEP", "v").expect("set");
    }
    fs::remove_file(secret_store_path(home.path())).unwrap();

    let error = SecretStore::open_or_create(home.path()).expect_err("must error");
    assert!(
        matches!(&error, StackError::AgeKeyParse { reason, .. } if reason.contains("ciphertext is missing")),
        "got: {error:?}",
    );
}

#[test]
fn open_or_create_rejects_store_without_age_key() {
    let home = tempfile::tempdir().expect("tempdir");
    {
        let mut store = SecretStore::open_or_create(home.path()).expect("init");
        store.set("KEEP", "v").expect("set");
    }
    fs::remove_file(age_key_path(home.path())).unwrap();

    let error = SecretStore::open_or_create(home.path()).expect_err("must error");
    assert!(
        matches!(&error, StackError::SecretStoreRead { source, .. } if source.to_string().contains("age key is missing")),
        "got: {error:?}",
    );
}

#[test]
fn second_init_preserves_pre_existing_secrets() {
    let home = tempfile::tempdir().expect("tempdir");
    {
        let mut store = SecretStore::open_or_create(home.path()).expect("init");
        store.set("KEEP", "value").expect("set");
    }
    let store = SecretStore::open_or_create(home.path()).expect("reopen");
    assert_eq!(store.get("KEEP").unwrap(), "value");
}
