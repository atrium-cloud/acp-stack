use super::StackError;
use std::path::PathBuf;

#[test]
fn stack_update_binary_swap_reports_rollback_outcome() {
    let io = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    let clean = StackError::StackUpdateBinarySwap {
        path: PathBuf::from("/opt/acps/acps"),
        source: io(),
        rollback_errors: Vec::new(),
    };
    assert_eq!(clean.error_code(), "stack.update_binary_swap_failed");
    assert_eq!(
        clean.to_string(),
        "failed to replace /opt/acps/acps during stack update binary swap: denied"
    );

    let broken = StackError::StackUpdateBinarySwap {
        path: PathBuf::from("/opt/acps/acps"),
        source: io(),
        rollback_errors: vec!["failed to restore /opt/acps/acps: denied".to_owned()],
    };
    assert_eq!(broken.error_code(), "stack.update_binary_swap_failed");
    assert_eq!(
        broken.to_string(),
        "failed to replace /opt/acps/acps during stack update binary swap: denied; rollback errors: failed to restore /opt/acps/acps: denied"
    );
    assert!(!broken.public_message().contains("/opt"));
}

#[test]
fn workspace_command_failure_display_formats_exit_status_plainly() {
    let exited = StackError::WorkspaceCommandFailed {
        command: "git clone",
        exit: Some(128),
        stderr_tail: "repository not found".to_owned(),
    }
    .to_string();
    assert_eq!(
        exited,
        "`git clone` exited with status 128: repository not found"
    );
    assert!(
        !exited.contains("Some("),
        "exit status must not expose Option debug formatting: {exited}"
    );

    let signaled = StackError::WorkspaceCommandFailed {
        command: "git clone",
        exit: None,
        stderr_tail: "terminated by signal".to_owned(),
    }
    .to_string();
    assert_eq!(
        signaled,
        "`git clone` exited without a status: terminated by signal"
    );
}

const CANARY_PATH: &str = "/home/operator/canary";
const CANARY_SECRET: &str = "sk-canary-secret";

fn assert_public_message_excludes(err: &StackError, needles: &[&str]) -> String {
    let message = err.public_message();
    for needle in needles {
        assert!(
            !message.contains(needle),
            "public message must not contain `{needle}`: {message}"
        );
    }
    message
}

#[test]
fn node_runtime_errors_report_codes_and_sanitized_messages() {
    let install = StackError::NodeRuntimeInstallFailed {
        reason: format!("rename {CANARY_PATH} failed: permission denied"),
    };
    assert_eq!(install.error_code(), "node_runtime.install_failed");
    assert_eq!(
        install.http_status(),
        http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let message = assert_public_message_excludes(&install, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "managed Node.js runtime install failed");

    let mismatch = StackError::NodeRuntimeChecksumMismatch {
        archive: "node-v26.0.0-linux-x64.tar.gz".to_owned(),
        expected: "a".repeat(64),
        actual: "b".repeat(64),
    };
    assert_eq!(mismatch.error_code(), "node_runtime.checksum_mismatch");
    assert_eq!(mismatch.http_status(), http::StatusCode::BAD_GATEWAY);
    assert!(
        mismatch
            .public_message()
            .contains("node-v26.0.0-linux-x64.tar.gz")
    );
}

#[test]
fn agent_test_failed_public_message_drops_reason_and_keeps_stage() {
    let err = StackError::AgentTestFailed {
        stage: "ACP initialize".to_owned(),
        reason: format!("spawn /agent --root {CANARY_PATH} failed"),
        code: "spawn_failed",
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "--root"]);
    assert_eq!(message, "agent test failed at ACP initialize");
}

#[test]
fn agent_initialize_failed_public_message_drops_free_text_reason() {
    let err = StackError::AgentInitializeFailed {
        reason: format!("installer thread join failed: write {CANARY_PATH}: permission denied"),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "agent failed to initialize");
}

#[test]
fn agent_switch_conflict_public_message_keeps_identifier_reason() {
    let err = StackError::AgentSwitchConflict {
        reason: "an earlier switch to `target-b` did not finish (phase `swap`)".to_owned(),
    };
    let message = err.public_message();
    assert!(
        message.contains("target-b"),
        "identifier reason must survive: {message}"
    );
}

#[test]
fn agent_installer_binary_unrunnable_public_message_drops_io_error() {
    let err = StackError::AgentInstallerBinaryUnrunnable {
        path: PathBuf::from(CANARY_PATH),
        source: std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("cannot execute {CANARY_PATH}"),
        ),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "cannot execute"]);
    assert_eq!(
        message,
        "agent installer produced a binary that cannot be spawned on this host"
    );
}

#[test]
fn agent_installer_log_persist_public_message_drops_path() {
    let err = StackError::AgentInstallerLogPersist {
        path: PathBuf::from(CANARY_PATH),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH]);
    assert_eq!(message, "failed to persist the installer log");
}

#[test]
fn skill_install_target_conflict_public_message_drops_path_keeps_reason() {
    let err = StackError::SkillInstallTargetConflict {
        path: PathBuf::from(CANARY_PATH),
        reason: "target exists but is not a directory".to_owned(),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH]);
    assert_eq!(
        message,
        "skill install target conflict: target exists but is not a directory"
    );
}

#[test]
fn skill_install_failed_public_message_drops_free_text_reason() {
    let err = StackError::SkillInstallFailed {
        reason: format!("stat skill target `{CANARY_PATH}`: permission denied"),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "skill install failed");
}

#[test]
fn registry_load_public_message_drops_free_text_reason() {
    let err = StackError::RegistryLoad {
        reason: format!("failed to read operator override {CANARY_PATH}: permission denied"),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "agent registry could not be loaded");
}

#[test]
fn init_run_corrupted_public_message_drops_free_text_reason() {
    let err = StackError::InitRunCorrupted {
        reason: format!("init run 7 has invalid args_json: invalid type: string \"{CANARY_PATH}\""),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH]);
    assert_eq!(message, "init run state is corrupted");
}

#[test]
fn agent_install_all_paths_failed_public_message_keeps_composed_summary() {
    let err = StackError::AgentInstallAllPathsFailed {
        summary: "shell: agent installer exited with status 7; npm: skipped, missing tools: npm"
            .to_owned(),
    };
    let message = err.public_message();
    assert!(
        message.contains("shell: agent installer exited with status 7"),
        "composed per-path summary must survive: {message}"
    );
}

#[test]
fn github_release_archive_extract_public_message_drops_reason_keeps_repo() {
    let err = StackError::GithubReleaseArchiveExtract {
        repo: "owner/repo".to_owned(),
        reason: format!("failed to create destination dir {CANARY_PATH}: permission denied"),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "failed to extract release archive from owner/repo");
}

#[test]
fn workspace_destination_errors_public_message_drops_paths() {
    let not_empty = StackError::WorkspaceDestinationNotEmpty {
        dest: CANARY_PATH.to_owned(),
    };
    let message = assert_public_message_excludes(&not_empty, &[CANARY_PATH]);
    assert_eq!(
        message,
        "workspace destination is not empty and is not a known acp-stack source directory"
    );

    let outside = StackError::WorkspaceDestinationOutsideRoot {
        dest: CANARY_PATH.to_owned(),
        root: format!("{CANARY_PATH}/root"),
    };
    let message = assert_public_message_excludes(&outside, &[CANARY_PATH]);
    assert_eq!(message, "workspace destination is outside workspace.root");
}

#[test]
fn workspace_materialize_failed_public_message_drops_free_text_reason() {
    let err = StackError::WorkspaceMaterializeFailed {
        reason: format!("could not create `{CANARY_PATH}`: permission denied"),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "workspace materialization failed");
}

#[test]
fn workspace_command_failed_public_message_drops_stderr() {
    let exited = StackError::WorkspaceCommandFailed {
        command: "git clone",
        exit: Some(128),
        stderr_tail: format!("fatal: could not write {CANARY_PATH}"),
    };
    let message = assert_public_message_excludes(&exited, &[CANARY_PATH, "fatal"]);
    assert_eq!(message, "`git clone` exited with status 128");

    let signaled = StackError::WorkspaceCommandFailed {
        command: "git clone",
        exit: None,
        stderr_tail: format!("terminated while writing {CANARY_PATH}"),
    };
    let message = assert_public_message_excludes(&signaled, &[CANARY_PATH, "terminated"]);
    assert_eq!(message, "`git clone` exited without a status");
}

#[test]
fn workspace_source_invalid_public_message_drops_free_text_reason() {
    let code = StackError::WorkspaceCodeSourceInvalid {
        index: 0,
        reason: format!("credential_ref `{CANARY_SECRET}` is not a valid secret reference"),
    };
    let message = assert_public_message_excludes(&code, &[CANARY_SECRET]);
    assert_eq!(message, "workspace.code_sources[0] is invalid");

    let data = StackError::WorkspaceDataSourceInvalid {
        index: 1,
        reason: format!("local path `{CANARY_PATH}` is not readable: permission denied"),
    };
    let message = assert_public_message_excludes(&data, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "workspace.data_sources[1] is invalid");
}

#[test]
fn workspace_not_found_public_messages_keep_relative_requested_path() {
    let missing = StackError::WorkspaceNotFound {
        requested: "notes/x.txt".to_owned(),
    };
    assert_eq!(
        missing.public_message(),
        "workspace path `notes/x.txt` was not found"
    );

    let missing_parent = StackError::WorkspaceParentNotFound {
        requested: "notes/x.txt".to_owned(),
    };
    assert_eq!(
        missing_parent.public_message(),
        "workspace parent directory for `notes/x.txt` was not found"
    );
}

#[test]
fn command_cwd_outside_workspace_public_message_drops_path() {
    let err = StackError::CommandCwdOutsideWorkspace {
        requested: CANARY_PATH.to_owned(),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH]);
    assert_eq!(message, "command cwd resolves outside the workspace root");
}

#[test]
fn invalid_secret_ref_name_public_message_drops_offending_name() {
    let err = StackError::InvalidSecretRefName {
        name: CANARY_SECRET.to_owned(),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_SECRET]);
    assert_eq!(
        message,
        "a secret ref name is invalid; use ASCII letters, digits, and underscores, and do not start with a digit"
    );
}

#[test]
fn invalid_secret_ref_name_display_matches_public_guidance() {
    let err = StackError::InvalidSecretRefName {
        name: "9lives".to_owned(),
    };
    let display = err.to_string();
    assert!(
        display.contains("do not start with a digit"),
        "Display guidance must cover the leading-digit rule: {display}"
    );
}

#[test]
fn archive_read_failed_public_message_drops_free_text_reason() {
    let err = StackError::ArchiveReadFailed {
        reason: format!("could not open `{CANARY_PATH}`: permission denied"),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "archive read failed");
}

#[test]
fn safe_download_failed_public_message_drops_reason_keeps_url() {
    let err = StackError::SafeDownloadFailed {
        url: "https://example.com/archive.tar.gz".to_owned(),
        reason: format!("could not create destination `{CANARY_PATH}`: permission denied"),
    };
    let message = assert_public_message_excludes(&err, &[CANARY_PATH, "permission denied"]);
    assert_eq!(
        message,
        "download from https://example.com/archive.tar.gz failed"
    );
}

#[test]
fn download_public_messages_strip_url_credentials() {
    let leaky = format!("https://user:{CANARY_SECRET}@example.com/path?token={CANARY_SECRET}#frag");

    let failed = StackError::SafeDownloadFailed {
        url: leaky.clone(),
        reason: "read failed".to_owned(),
    };
    let message =
        assert_public_message_excludes(&failed, &[CANARY_SECRET, "user", "token", "frag"]);
    assert_eq!(message, "download from https://example.com/path failed");

    let status = StackError::SafeDownloadHttpStatus {
        url: leaky.clone(),
        status: 403,
    };
    let message =
        assert_public_message_excludes(&status, &[CANARY_SECRET, "user", "token", "frag"]);
    assert_eq!(
        message,
        "download from https://example.com/path failed with HTTP status 403"
    );

    let redirect = StackError::SafeDownloadInsecureRedirect { url: leaky };
    let message = assert_public_message_excludes(&redirect, &[CANARY_SECRET, "token", "frag"]);
    assert_eq!(
        message,
        "download URL `https://example.com/path` is not allowed (only https:// is permitted)"
    );
}

#[test]
fn download_public_message_renders_bare_origin_without_trailing_slash() {
    let err = StackError::SafeDownloadFailed {
        url: "https://example.com".to_owned(),
        reason: "connect failed".to_owned(),
    };
    assert_eq!(
        err.public_message(),
        "download from https://example.com failed"
    );
}

#[test]
fn download_public_messages_fall_back_when_url_is_missing_or_unparseable() {
    let empty = StackError::SafeDownloadFailed {
        url: String::new(),
        reason: "connect failed".to_owned(),
    };
    assert_eq!(empty.public_message(), "download failed");

    let unparseable = StackError::SafeDownloadFailed {
        url: "not a url".to_owned(),
        reason: "connect failed".to_owned(),
    };
    let message = assert_public_message_excludes(&unparseable, &["not a url"]);
    assert_eq!(message, "download failed");

    let empty_status = StackError::SafeDownloadHttpStatus {
        url: String::new(),
        status: 502,
    };
    assert_eq!(
        empty_status.public_message(),
        "download failed with HTTP status 502"
    );
}

// === domain coverage ===

const ENUM_SOURCE: &str = include_str!("../error.rs");
const DISPATCH_SOURCE: &str = include_str!("dispatch.rs");

/// Every domain module `dispatch.rs` consults, paired with its source so the
/// coverage test fails when a variant is added without a claiming arm.
const DOMAIN_MODULES: &[(&str, &str)] = &[
    ("config", include_str!("config.rs")),
    ("state", include_str!("state.rs")),
    ("security", include_str!("security.rs")),
    ("secrets", include_str!("secrets.rs")),
    ("supabase", include_str!("supabase.rs")),
    ("edge", include_str!("edge.rs")),
    ("extensions", include_str!("extensions.rs")),
    ("workspace_source", include_str!("workspace_source.rs")),
    ("download", include_str!("download.rs")),
    ("archive", include_str!("archive.rs")),
    ("node_runtime", include_str!("node_runtime.rs")),
    ("serve", include_str!("serve.rs")),
    ("agent_install", include_str!("agent_install.rs")),
    ("agent_runtime", include_str!("agent_runtime.rs")),
    ("session", include_str!("session.rs")),
    ("workspace", include_str!("workspace.rs")),
    ("command", include_str!("command.rs")),
    ("permission", include_str!("permission.rs")),
    ("auth_http", include_str!("auth_http.rs")),
];

const DISPATCH_FUNCTIONS: &[&str] = &["error_code", "public_message", "http_status"];

/// Variants the dispatcher resolves before consulting any domain module.
const DISPATCH_EARLY_RETURNS: &[(&str, &str)] =
    &[("error_code", "NativeAgentConfigOperationFailed")];

fn stack_error_variants() -> Vec<&'static str> {
    let start = ENUM_SOURCE
        .find("pub enum StackError {")
        .expect("StackError enum declaration");
    let body = &ENUM_SOURCE[start..];
    let end = body.find("\n}\n").expect("StackError enum close");
    body[..end]
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("    ")?;
            if rest.starts_with(' ') || rest.starts_with('#') || rest.starts_with('/') {
                return None;
            }
            let name_end = rest
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(rest.len());
            let name = &rest[..name_end];
            let first = name.chars().next()?;
            first.is_ascii_uppercase().then_some(name)
        })
        .collect()
}

fn function_body<'a>(source: &'a str, function: &str) -> &'a str {
    let signature = format!("fn {function}(");
    let start = source
        .find(&signature)
        .unwrap_or_else(|| panic!("domain module lacks `{signature}`"));
    let body = &source[start..];
    let end = body.find("\n}\n").unwrap_or(body.len());
    &body[..end]
}

fn claimed_variants(body: &str, variants: &[&'static str]) -> Vec<&'static str> {
    variants
        .iter()
        .copied()
        .filter(|variant| {
            body.match_indices(variant).any(|(index, _)| {
                let before = body[..index].chars().next_back();
                let after = body[index + variant.len()..].chars().next();
                let boundary_before = before.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
                let boundary_after = after.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
                boundary_before && boundary_after
            })
        })
        .collect()
}

#[test]
fn every_stack_error_variant_is_claimed_by_exactly_one_domain() {
    let variants = stack_error_variants();
    // thiserror requires one `#[error(...)]` per variant, so the attribute
    // count anchors the line scanner against silently dropped declarations.
    let error_attributes = ENUM_SOURCE
        .lines()
        .filter(|line| line.starts_with("    #[error"))
        .count();
    assert_eq!(
        variants.len(),
        error_attributes,
        "variant scan dropped declarations: {} variants vs {error_attributes} #[error] attributes",
        variants.len()
    );
    let mut problems = Vec::new();
    for function in DISPATCH_FUNCTIONS {
        let chain = function_body(DISPATCH_SOURCE, function);
        for (module, _) in DOMAIN_MODULES {
            if !chain.contains(&format!("{module}::{function}(self)")) {
                problems.push(format!(
                    "{function}: dispatch chain does not consult `{module}`"
                ));
            }
        }
        let mut claims: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        for (module, source) in DOMAIN_MODULES {
            let body = function_body(source, function);
            for variant in claimed_variants(body, &variants) {
                claims.entry(variant).or_default().push(module);
            }
        }
        for variant in &variants {
            let early = DISPATCH_EARLY_RETURNS.contains(&(function, variant));
            match claims.get(variant).map(Vec::as_slice) {
                None if early => {}
                None => problems.push(format!("{function}: `{variant}` is claimed by no domain")),
                Some([_]) => {}
                Some(modules) => problems.push(format!(
                    "{function}: `{variant}` is claimed by several domains: {modules:?}"
                )),
            }
        }
    }
    assert!(
        problems.is_empty(),
        "StackError domain coverage problems:\n{}",
        problems.join("\n")
    );
}

#[test]
fn dispatch_fallback_matches_envelope_internal_error() {
    let internal = crate::envelope::ApiError::for_status(http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(internal.code, crate::envelope::INTERNAL_ERROR_CODE);
    assert_eq!(internal.message, crate::envelope::INTERNAL_ERROR_MESSAGE);
}

#[test]
fn newly_claimed_variants_report_codes_and_sanitized_messages() {
    let table_prefix = StackError::InvalidSupabaseTablePrefix {
        prefix: "9bad".to_owned(),
    };
    assert_eq!(
        table_prefix.error_code(),
        "logging.supabase.invalid_table_prefix"
    );
    assert_eq!(table_prefix.http_status(), http::StatusCode::BAD_REQUEST);
    assert_public_message_excludes(&table_prefix, &["9bad"]);

    let cli = StackError::SupabaseCliFailed {
        command: format!("supabase db push --workdir {CANARY_PATH}"),
        status: "1".to_owned(),
        stderr_tail: format!("token {CANARY_SECRET} rejected"),
    };
    assert_eq!(cli.error_code(), "logging.supabase.cli_failed");
    assert_eq!(cli.http_status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    let message = assert_public_message_excludes(&cli, &[CANARY_PATH, CANARY_SECRET, "db push"]);
    assert_eq!(message, "Supabase CLI setup failed");

    let sandbox = StackError::SandboxFailed {
        reason: format!("bind mount {CANARY_PATH}: permission denied"),
    };
    assert_eq!(sandbox.error_code(), "serve.sandbox_failed");
    assert_eq!(
        sandbox.http_status(),
        http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let message = assert_public_message_excludes(&sandbox, &[CANARY_PATH, "permission denied"]);
    assert_eq!(message, "sandbox setup failed");

    let push = StackError::ProviderSecretNotPushDeliverable {
        provider_id: "openrouter".to_owned(),
        env_ref: "OPENROUTER_API_KEY".to_owned(),
    };
    assert_eq!(
        push.error_code(),
        "secrets.provider_secret_not_push_deliverable"
    );
    assert_eq!(push.http_status(), http::StatusCode::BAD_REQUEST);
    let message = push.public_message();
    assert!(message.contains("openrouter") && message.contains("OPENROUTER_API_KEY"));

    let catalog = StackError::ProviderModelCatalog {
        provider: "openrouter".to_owned(),
        reason: format!("GET https://x.example/models?key={CANARY_SECRET} failed"),
    };
    assert_eq!(catalog.error_code(), "agent.provider_model_catalog_failed");
    assert_eq!(catalog.http_status(), http::StatusCode::BAD_GATEWAY);
    let message = assert_public_message_excludes(&catalog, &[CANARY_SECRET, "x.example"]);
    assert_eq!(message, "provider `openrouter` model catalog fetch failed");

    let array = StackError::ArrayTargetsFailed {
        action: "start",
        failed: 2,
        total: 3,
        summary: format!("target-b: spawn {CANARY_PATH} failed"),
    };
    assert_eq!(array.error_code(), "array.targets_failed");
    assert_eq!(array.http_status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    let message = assert_public_message_excludes(&array, &[CANARY_PATH]);
    assert_eq!(message, "array start failed for 2 of 3 target(s)");

    let home = StackError::HomeNotIsolated {
        path: PathBuf::from(CANARY_PATH),
    };
    assert_eq!(home.error_code(), "config.home_not_isolated");
    assert_eq!(home.http_status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_public_message_excludes(&home, &[CANARY_PATH]);

    let egress = StackError::FixtureEgressRefused {
        url: "https://api.example.com/v1".to_owned(),
    };
    assert_eq!(egress.error_code(), "config.fixture_egress_refused");
    assert_eq!(
        egress.http_status(),
        http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_public_message_excludes(&egress, &["api.example.com"]);
}
