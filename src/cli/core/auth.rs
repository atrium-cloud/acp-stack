use super::*;

pub(crate) const SESSION_KEY_ENV: &str = "ACP_STACK_SESSION_KEY";

pub(crate) fn resolve_session_key(value: Option<String>) -> Result<String> {
    if let Some(key) = value {
        return validate_key_input("--session-key", key);
    }
    if let Ok(key) = std::env::var(SESSION_KEY_ENV) {
        return validate_key_input(SESSION_KEY_ENV, key);
    }
    Err(StackError::MissingField {
        field: "--session-key or ACP_STACK_SESSION_KEY",
    })
}

pub(crate) enum SessionAccess {
    Bearer(String),
    Local,
}

pub(crate) fn resolve_session_access(
    config: &Config,
    value: Option<String>,
) -> Result<SessionAccess> {
    if let Some(key) = value {
        return validate_key_input("--session-key", key).map(SessionAccess::Bearer);
    }
    if let Ok(key) = std::env::var(SESSION_KEY_ENV) {
        return validate_key_input(SESSION_KEY_ENV, key).map(SessionAccess::Bearer);
    }
    if config.local.session_auth == crate::config::LocalSessionAuth::Keyless {
        return Ok(SessionAccess::Local);
    }
    Err(StackError::MissingField {
        field: "--session-key or ACP_STACK_SESSION_KEY (or enable local session access with `acps auth local-session-access enable`)",
    })
}

pub(crate) fn resolve_admin_key(value: Option<String>, interactive: bool) -> Result<String> {
    if let Some(key) = value {
        return validate_key_input("--admin-key", key);
    }
    if interactive && std::io::stdin().is_terminal() {
        let key = rpassword::prompt_password("admin key: ")
            .map_err(|source| StackError::ServeIo { source })?;
        return validate_key_input("--admin-key", key);
    }
    Err(StackError::MissingField {
        field: "--admin-key",
    })
}

pub(crate) fn validate_local_admin_key(key: &str) -> Result<()> {
    let home = home_dir()?;
    let loaded_config = Config::load_from_default_path_with_legacy()?;
    let state_path = default_state_path(&home);
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    match ensure_auth_verifier_pair(&store, loaded_config.legacy_auth.as_ref(), &home)? {
        AuthVerifierEnsureOutcome::Preserved
        | AuthVerifierEnsureOutcome::BackfilledLegacySecrets => {}
        AuthVerifierEnsureOutcome::Missing => {
            return Err(StackError::MissingField {
                field: "auth_keys.session and auth_keys.admin",
            });
        }
    }
    validate_admin_key_against_store(&store, key)
}

pub(crate) fn validate_local_admin_key_from_state(key: &str) -> Result<()> {
    let home = home_dir()?;
    let state_path = default_state_path(&home);
    let store = StateStore::open(&state_path)?;
    store.migrate()?;
    validate_admin_key_against_store(&store, key)
}

fn validate_admin_key_against_store(store: &StateStore, key: &str) -> Result<()> {
    let verifiers = store.load_auth_verifier_pair()?;
    if verifiers.verify(key) == Some(KeyKind::Admin) {
        Ok(())
    } else {
        Err(StackError::InvalidParam {
            field: "--admin-key",
            reason: "admin key did not validate against local auth verifier".to_owned(),
        })
    }
}

fn validate_key_input(field: &'static str, value: String) -> Result<String> {
    if value.trim().is_empty() || value.trim().len() != value.len() {
        return Err(StackError::MissingField { field });
    }
    Ok(value)
}
