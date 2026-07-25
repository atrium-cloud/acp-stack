use super::*;

fn opencode_config(provider: &str, model: &str) -> Config {
    let mut config = crate::config::load_config_from_str(include_str!(
        "../../../../tests/fixtures/valid-opencode-stack.toml"
    ))
    .expect("fixture config");
    config.agent.env.clear();
    apply_mapped_agent_provider(&mut config, provider, None).expect("provider");
    config.agent.provider.as_mut().expect("provider").model = Some(model.to_owned());
    config
}

#[test]
fn default_selection_preserves_canonical_provider_and_selected_values_replace_it() {
    let current = opencode_config("openrouter", "openrouter/old-model");
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"model":"openai/new-model","theme":"dark"}"#,
    )
    .expect("inspect");
    let default = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: Vec::new(),
            executable_settings_acknowledged: false,
        },
        &current,
        Path::new("/tmp/home"),
    )
    .expect("prepare default");
    assert_eq!(
        default
            .canonical_config
            .agent
            .provider
            .as_ref()
            .expect("provider")
            .id,
        "openrouter"
    );
    assert_eq!(
        default
            .canonical_config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref()),
        Some("openrouter/old-model")
    );

    let selected = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: vec!["provider".to_owned(), "model".to_owned()],
            executable_settings_acknowledged: false,
        },
        &current,
        Path::new("/tmp/home"),
    )
    .expect("prepare selected");
    let provider = selected
        .canonical_config
        .agent
        .provider
        .as_ref()
        .expect("provider");
    assert_eq!(provider.id, "openai");
    assert_eq!(provider.model.as_deref(), Some("openai/new-model"));
    assert!(
        selected
            .canonical_config
            .agent
            .env
            .iter()
            .any(|name| name == "OPENAI_API_KEY")
    );

    let error = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: vec!["model".to_owned()],
            executable_settings_acknowledged: false,
        },
        &current,
        Path::new("/tmp/home"),
    )
    .err()
    .expect("model/provider mismatch");
    assert_eq!(error.error_code(), "native_config_model_provider_mismatch");
}

#[test]
fn provider_import_preserves_structured_catalog_credentials_through_rebase() {
    let home = tempfile::tempdir().expect("home");
    let mut current = opencode_config("openrouter", "openrouter/old-model");
    current.agent.env.clear();
    current
        .agent
        .provider
        .as_mut()
        .expect("provider")
        .api_key_ref = None;
    current.agent.providers = Some(crate::config::AgentProvidersConfig {
        active: vec!["openrouter".to_owned()],
        selected_aliases: BTreeMap::new(),
    });
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"model":"openai/gpt-5.5"}"#,
    )
    .expect("inspect");

    let mut prepared = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: vec!["provider".to_owned(), "model".to_owned()],
            executable_settings_acknowledged: false,
        },
        &current,
        home.path(),
    )
    .expect("prepare");

    let provider = prepared
        .canonical_config
        .agent
        .provider
        .as_ref()
        .expect("provider");
    assert_eq!(provider.id, "openai");
    assert!(provider.api_key_ref.is_none());
    assert!(
        !prepared
            .canonical_config
            .agent
            .env
            .iter()
            .any(|name| name == "OPENAI_API_KEY")
    );
    assert_eq!(
        prepared
            .canonical_config
            .agent
            .providers
            .as_ref()
            .expect("provider settings")
            .active,
        ["openrouter", "openai"]
    );

    let mut later = current.clone();
    later.logging.level = "debug".to_owned();
    rebase_prepared_native_config_import(&mut prepared, &later).expect("rebase");
    assert_eq!(prepared.canonical_config.logging.level, "debug");
    assert_eq!(
        prepared
            .canonical_config
            .agent
            .providers
            .as_ref()
            .expect("provider settings")
            .active,
        ["openrouter", "openai"]
    );

    let credential = |name: &str| {
        crate::secrets::ProviderCredentialSet::aliasless(crate::secrets::ProviderCredential::new(
            BTreeMap::from([(name.to_owned(), "secret".to_owned())]),
            BTreeMap::new(),
        ))
    };
    let mut secrets = SecretStore::open_or_create(home.path()).expect("secret store");
    secrets
        .replace_provider_credentials(
            BTreeMap::from([
                ("openrouter".to_owned(), credential("OPENROUTER_API_KEY")),
                ("openai".to_owned(), credential("OPENAI_API_KEY")),
            ]),
            &[],
        )
        .expect("catalog");
    validate_native_config_secret_refs(&prepared, home.path()).expect("validate catalog");
}

#[test]
fn executable_acknowledgement_is_revision_bound() {
    let current = opencode_config("openrouter", "openrouter/old-model");
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"plugin":["file:///tmp/plugin.js"],"theme":"dark"}"#,
    )
    .expect("inspect");
    let error = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: Vec::new(),
            executable_settings_acknowledged: false,
        },
        &current,
        Path::new("/tmp/home"),
    )
    .err()
    .expect("ack required");
    assert_eq!(error.error_code(), "native_config_executable_ack_required");

    let error = validate_native_config_selection(
        &inspected,
        &NativeConfigSelection {
            revision: "different".to_owned(),
            selected_managed_field_ids: Vec::new(),
            executable_settings_acknowledged: true,
        },
    )
    .unwrap_err();
    assert_eq!(error.error_code(), "native_config_revision_mismatch");
}

#[test]
fn opencode_lsp_requires_executable_acknowledgement() {
    let current = opencode_config("openrouter", "openrouter/old-model");
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"lsp":{"custom":{"command":["/tmp/custom-lsp"]}}}"#,
    )
    .expect("inspect");
    assert!(
        inspected
            .inspection()
            .executable_categories
            .contains(&ExecutableCategory::CommandHelpers)
    );
    let error = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: Vec::new(),
            executable_settings_acknowledged: false,
        },
        &current,
        Path::new("/tmp/home"),
    )
    .err()
    .expect("LSP command requires acknowledgement");
    assert_eq!(error.error_code(), "native_config_executable_ack_required");
}

#[test]
fn executable_mcp_acknowledgement_is_required_only_when_selected() {
    let current = opencode_config("openrouter", "openrouter/old-model");
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"mcp":{"local":{"type":"local","command":["echo","ok"]}},"theme":"dark"}"#,
    )
    .expect("inspect");
    prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: Vec::new(),
            executable_settings_acknowledged: false,
        },
        &current,
        Path::new("/tmp/home"),
    )
    .expect("unselected executable candidate is removed");

    let error = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: vec!["mcp:local".to_owned()],
            executable_settings_acknowledged: false,
        },
        &current,
        Path::new("/tmp/home"),
    )
    .err()
    .expect("selected executable candidate requires acknowledgement");
    assert_eq!(error.error_code(), "native_config_executable_ack_required");
}

#[test]
fn unmappable_mcp_never_survives_in_residual() {
    let inspected = inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{
                "mcp":{"remote":{"url":"https://example.com","headers":{"Authorization":"literal"}}},
                "theme":"dark"
            }"#,
        )
        .expect("inspect");
    assert!(inspected.inspection().blocked_fields.iter().any(|field| {
        field.path == "mcp.remote" && field.reason == BlockedReason::McpUnmappable
    }));
    let residual: JsonValue = serde_json::from_slice(inspected.residual()).expect("json");
    assert!(residual.get("mcp").is_none());
}

#[test]
fn mcp_urls_and_arguments_cannot_embed_literal_credentials() {
    let inspected = inspect_native_config(
        "codex",
        Some("config.toml"),
        r#"
[mcp_servers.remote]
url = "https://example.com/sse?access_token=literal"

[mcp_servers.local]
command = "server"
args = ["--api-key", "literal"]

[mcp_servers.positional]
command = "server"
args = ["ghp_16charsofpayload"]

[mcp_servers.benign]
command = "server"
args = ["sk-learn"]
"#,
    )
    .expect("inspect");
    for path in [
        "mcp_servers.remote",
        "mcp_servers.local",
        "mcp_servers.positional",
    ] {
        assert!(
            inspected
                .inspection()
                .blocked_fields
                .iter()
                .any(|field| { field.path == path && field.reason == BlockedReason::Credentials })
        );
    }
    assert!(
        inspected
            .inspection()
            .managed_fields
            .iter()
            .any(|field| field.id == "mcp:benign")
    );
}

#[test]
fn manifest_path_collections_are_bounded() {
    let mut root = JsonMap::new();
    for index in 0..(MAX_MANIFEST_PATHS + 50) {
        root.insert(
            format!("credential_{index}"),
            JsonValue::String("x".to_owned()),
        );
    }
    let source = serde_json::to_string(&root).expect("json");
    let inspected =
        inspect_native_config("opencode", Some("opencode.json"), &source).expect("inspect");
    assert_eq!(
        inspected.inspection().blocked_fields.len(),
        MAX_MANIFEST_PATHS
    );
    assert!(
        inspected
            .inspection()
            .warnings
            .contains(&"manifest-truncated".to_owned())
    );
}

#[test]
fn rebase_keeps_selected_values_and_accepts_later_canonical_changes() {
    let current = opencode_config("openrouter", "openrouter/old-model");
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"model":"openai/new-model","theme":"dark"}"#,
    )
    .expect("inspect");
    let mut prepared = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: vec!["provider".to_owned(), "model".to_owned()],
            executable_settings_acknowledged: false,
        },
        &current,
        Path::new("/tmp/home"),
    )
    .expect("prepare");
    let mut later = current;
    later.logging.level = "debug".to_owned();
    rebase_prepared_native_config_import(&mut prepared, &later).expect("rebase");
    assert_eq!(prepared.canonical_config.logging.level, "debug");
    let provider = prepared
        .canonical_config
        .agent
        .provider
        .as_ref()
        .expect("provider");
    assert_eq!(provider.id, "openai");
    assert_eq!(provider.model.as_deref(), Some("openai/new-model"));
}

#[test]
fn journal_round_trip_keeps_prepared_transaction_without_raw_manifest_values() {
    let home = tempfile::tempdir().expect("home");
    let config_path = home
        .path()
        .join(".config")
        .join("acp-stack")
        .join("acps-config.toml");
    let state_path = home
        .path()
        .join(".local")
        .join("share")
        .join("acp-stack")
        .join("state.sqlite");
    let current = opencode_config("openrouter", "openrouter/old-model");
    let inspected = inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"theme":"private-setting"}"#,
    )
    .expect("inspect");
    let prepared = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: Vec::new(),
            executable_settings_acknowledged: false,
        },
        &current,
        home.path(),
    )
    .expect("prepare");
    let record = NativeConfigOperationRecord {
        operation: NativeConfigOperation {
            operation_id: "nci_test_roundtrip".to_owned(),
            status: NativeConfigOperationStatus::Queued,
            harness: "opencode".to_owned(),
            revision: inspected.revision().to_owned(),
            agent_config: native_config_projection(&prepared.canonical_config),
            restart: NativeConfigRestartMetadata {
                required: true,
                queued: true,
                restarted: false,
                target_id: "opencode".to_owned(),
            },
            error: None,
        },
        transaction_fingerprint: prepared.transaction_fingerprint.clone(),
        prepared: Some(prepared),
        rollback_snapshots: Vec::new(),
        prior_config: None,
        prior_was_running: false,
        applied_file_digests: Vec::new(),
        applied_at: None,
        updated_at: chrono::Utc::now(),
        cancelled: false,
        phase: NativeConfigOperationPhase::Staged,
    };
    persist_native_config_operation(&state_path, &config_path, home.path(), &record)
        .expect("persist");
    let loaded =
        load_native_config_operation_journal(&state_path, &config_path, home.path()).expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].operation, record.operation);
    assert_eq!(
        loaded[0]
            .prepared
            .as_ref()
            .expect("prepared")
            .native_content,
        record.prepared.as_ref().expect("prepared").native_content
    );
}

#[test]
fn claude_snapshot_journal_excludes_auth_state_and_digest_ignores_it() {
    let home = tempfile::tempdir().expect("home");
    let claude_state = home.path().join(".claude.json");
    prepare_owner_managed_file_path(home.path(), &claude_state).expect("state path");
    atomic_write_owner_only(
        &claude_state,
        br#"{"oauthAccessToken":"never-persist-this","hasCompletedOnboarding":false}"#,
    )
    .expect("state write");
    let snapshots =
        capture_native_config_snapshots(std::slice::from_ref(&claude_state), home.path())
            .expect("snapshot");
    let digests =
        capture_native_config_file_digests(std::slice::from_ref(&claude_state), home.path())
            .expect("digest");

    let config = opencode_config("openrouter", "openrouter/old-model");
    let config_path = home
        .path()
        .join(".config")
        .join("acp-stack")
        .join("acps-config.toml");
    let state_path = home
        .path()
        .join(".local")
        .join("share")
        .join("acp-stack")
        .join("state.sqlite");
    let record = NativeConfigOperationRecord {
        operation: NativeConfigOperation {
            operation_id: "nci_claude_snapshot".to_owned(),
            status: NativeConfigOperationStatus::Failed,
            harness: "claude-code".to_owned(),
            revision: "revision".to_owned(),
            agent_config: native_config_projection(&config),
            restart: NativeConfigRestartMetadata {
                required: true,
                queued: true,
                restarted: false,
                target_id: "claude-code".to_owned(),
            },
            error: Some(NativeConfigOperationError {
                code: "native_config_rollback_failed".to_owned(),
            }),
        },
        transaction_fingerprint: "fingerprint".to_owned(),
        prepared: None,
        rollback_snapshots: snapshots.clone(),
        prior_config: Some(config),
        prior_was_running: true,
        applied_file_digests: digests.clone(),
        applied_at: None,
        updated_at: chrono::Utc::now(),
        cancelled: false,
        phase: NativeConfigOperationPhase::RollingBack,
    };
    persist_native_config_operation(&state_path, &config_path, home.path(), &record)
        .expect("persist");
    let journal = std::fs::read_to_string(
        state_path
            .parent()
            .expect("state parent")
            .join(JOURNAL_DIR_NAME)
            .join("nci_claude_snapshot.json"),
    )
    .expect("journal");
    assert!(!journal.contains("never-persist-this"));

    atomic_write_owner_only(
        &claude_state,
        br#"{"oauthAccessToken":"changed","hasCompletedOnboarding":false}"#,
    )
    .expect("unrelated state change");
    validate_native_config_file_digests(&digests, home.path())
        .expect("unrelated auth state is outside the owned digest");
    atomic_write_owner_only(
        &claude_state,
        br#"{"oauthAccessToken":"changed","hasCompletedOnboarding":true}"#,
    )
    .expect("owned state change");
    assert!(validate_native_config_file_digests(&digests, home.path()).is_err());
    restore_native_config_snapshots(&snapshots, home.path()).expect("restore");
    let restored: JsonValue =
        serde_json::from_slice(&std::fs::read(&claude_state).expect("restored state"))
            .expect("restored json");
    assert_eq!(restored["oauthAccessToken"], "changed");
    assert_eq!(restored["hasCompletedOnboarding"], false);
}

#[test]
fn semantic_replacement_and_snapshot_restore_are_atomic_at_file_boundary() {
    let home = tempfile::tempdir().expect("home");
    let config_path = home
        .path()
        .join(".config")
        .join("acp-stack")
        .join("acps-config.toml");
    let native_path = home
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    prepare_owner_managed_file_path(home.path(), &config_path).expect("config path");
    prepare_owner_managed_file_path(home.path(), &native_path).expect("native path");
    let current = opencode_config("openrouter", "openrouter/old-model");
    let canonical = current.to_canonical_toml().expect("canonical");
    atomic_write_owner_only(&config_path, canonical.as_bytes()).expect("config");
    atomic_write_owner_only(
        &native_path,
        br#"{"old_unmanaged":true,"model":"stale/model"}"#,
    )
    .expect("native");
    let inspected = inspect_native_config("opencode", Some("opencode.json"), r#"{"theme":"dark"}"#)
        .expect("inspect");
    let prepared = prepare_native_config_import(
        &inspected,
        &NativeConfigSelection {
            revision: inspected.revision().to_owned(),
            selected_managed_field_ids: Vec::new(),
            executable_settings_acknowledged: false,
        },
        &current,
        home.path(),
    )
    .expect("prepare");
    let paths = prepare_native_config_file_paths(&prepared, &config_path, home.path())
        .expect("prepare paths");
    let snapshots = capture_native_config_snapshots(&paths, home.path()).expect("snapshots");
    write_native_config_files(&prepared, &config_path, home.path()).expect("write");
    let written: JsonValue =
        serde_json::from_slice(&std::fs::read(&native_path).expect("read native")).expect("json");
    assert_eq!(written["theme"], "dark");
    assert_eq!(written["model"], "openrouter/old-model");
    assert!(written.get("old_unmanaged").is_none());

    restore_native_config_snapshots(&snapshots, home.path()).expect("restore");
    let restored: JsonValue =
        serde_json::from_slice(&std::fs::read(&native_path).expect("read restored")).expect("json");
    assert_eq!(restored["old_unmanaged"], true);
    assert_eq!(
        std::fs::read_to_string(&config_path).expect("config"),
        canonical
    );
}
