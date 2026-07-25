use acp_stack::config::{
    DEFAULT_COMMAND_PROGRESS_INTERVAL, load_config_from_str, parse_duration_string,
};

mod common;
use common::config::VALID_CONFIG;

#[test]
fn accepts_dependencies_section() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[dependencies]\ncommands = [{ name = \"git\", required = true }]\n\n[agent]",
    );
    let config = load_config_from_str(&updated).expect("dependencies parse");
    assert_eq!(config.dependencies.commands.len(), 1);
    assert_eq!(config.dependencies.commands[0].name, "git");
    assert!(config.dependencies.commands[0].required);
}

#[test]
fn rejects_removed_startup_section() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[[startup.scripts]]\nname = \"bootstrap\"\nscript = \"echo ready\"\nshell = \"/bin/sh\"\n\n[agent]",
    );
    let error = load_config_from_str(&updated).expect_err("startup scripts must be rejected");

    assert!(
        error.to_string().contains("`[startup]` was removed"),
        "got: {error}",
    );
}

#[test]
fn rejects_duplicate_dependency_names() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[dependencies]\ncommands = [{ name = \"git\" }, { name = \"git\" }]\n\n[agent]",
    );
    let error = load_config_from_str(&updated).expect_err("duplicate must fail");
    assert!(
        error
            .to_string()
            .contains("dependencies.commands contains duplicate"),
        "got: {error}",
    );
}

#[test]
fn accepts_stdio_mcp_server() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[[mcp.servers]]\ntype = \"stdio\"\nname = \"slack\"\ncommand = \"slack-mcp\"\nenv = [\"SLACK_BOT_TOKEN\"]\n\n[agent]",
    );
    let config = load_config_from_str(&updated).expect("stdio mcp parses");
    assert_eq!(config.mcp.servers.len(), 1);
    assert_eq!(config.mcp.servers[0].name(), "slack");
}

#[test]
fn rejects_duplicate_mcp_server_names() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[[mcp.servers]]\ntype = \"stdio\"\nname = \"slack\"\ncommand = \"a\"\n\n[[mcp.servers]]\ntype = \"stdio\"\nname = \"slack\"\ncommand = \"b\"\n\n[agent]",
    );
    let error = load_config_from_str(&updated).expect_err("duplicate names must fail");
    assert!(error.to_string().contains("duplicate name"), "got: {error}",);
}

#[test]
fn rejects_duplicate_mcp_server_names_across_kinds() {
    // Cross-transport name collisions (stdio + http with the same `name`) must
    // also be rejected: the agent identifies servers by name regardless of
    // transport, so allowing duplicates would silently overwrite the first
    // entry's wiring.
    let updated = VALID_CONFIG.replace(
        "[agent]",
        concat!(
            "[[mcp.servers]]\ntype = \"stdio\"\nname = \"shared\"\ncommand = \"a\"\n\n",
            "[[mcp.servers]]\ntype = \"http\"\nname = \"shared\"\nurl = \"https://example/x\"\n\n",
            "[agent]"
        ),
    );
    let error = load_config_from_str(&updated).expect_err("cross-kind duplicates must fail");
    assert!(error.to_string().contains("duplicate name"), "got: {error}",);
}

#[test]
fn rejects_http_mcp_with_bad_url() {
    let updated = VALID_CONFIG.replace(
        "[agent]",
        "[[mcp.servers]]\ntype = \"http\"\nname = \"linear\"\nurl = \"ftp://x\"\n\n[agent]",
    );
    let error = load_config_from_str(&updated).expect_err("bad url must fail");
    assert!(
        error
            .to_string()
            .contains("must start with http:// or https://"),
        "got: {error}",
    );
}

fn enable_supabase(input: &str) -> String {
    input.replace("enabled = false", "enabled = true")
}

#[test]
fn supabase_disabled_skips_url_check() {
    // VALID_CONFIG ships with enabled = false, so even a non-https url must
    // parse cleanly until external logging is actually turned on.
    let updated = VALID_CONFIG.replace(
        r#"url = "https://example.supabase.co""#,
        r#"url = "http://insecure.example""#,
    );
    let config = load_config_from_str(&updated).expect("disabled-supabase must parse");
    assert_eq!(
        config.logging.supabase.as_ref().map(|s| s.url.as_str()),
        Some("http://insecure.example")
    );
}

#[test]
fn supabase_enabled_requires_https() {
    let mut updated = enable_supabase(VALID_CONFIG);
    updated = updated.replace(
        r#"url = "https://example.supabase.co""#,
        r#"url = "http://example.supabase.co""#,
    );
    let error = load_config_from_str(&updated).expect_err("non-https supabase url must fail");
    assert!(
        error.to_string().contains("must start with `https://`"),
        "got: {error}",
    );
}

#[test]
fn supabase_enabled_schema_must_be_safe_identifier() {
    let updated = enable_supabase(VALID_CONFIG)
        .replace(r#"schema = "acp_stack""#, r#"schema = "drop tables;""#);
    let error = load_config_from_str(&updated).expect_err("unsafe schema must fail");
    assert!(
        error.to_string().contains("safe Postgres identifier"),
        "got: {error}",
    );
}

#[test]
fn supabase_enabled_with_clean_schema_and_https_passes() {
    let updated = enable_supabase(VALID_CONFIG);
    let config = load_config_from_str(&updated).expect("enabled-supabase happy path");
    let supabase = config.logging.supabase.expect("supabase set");
    assert!(supabase.enabled);
    assert_eq!(supabase.schema, "acp_stack");
}

#[test]
fn supabase_legacy_config_defaults_to_postgrest_backend() {
    let updated = enable_supabase(VALID_CONFIG);
    let config = load_config_from_str(&updated).expect("legacy supabase config parses");
    let supabase = config.logging.supabase.expect("supabase set");
    assert_eq!(
        supabase.backend,
        acp_stack::config::SupabaseLoggingBackend::Postgrest
    );
    assert_eq!(supabase.table_prefix, "");
    assert!(supabase.db_url_ref.is_none());
}

#[test]
fn supabase_postgres_backend_requires_db_url_ref() {
    let updated = enable_supabase(VALID_CONFIG).replace(
        "[logging.supabase]",
        "[logging.supabase]\nbackend = \"postgres\"",
    );
    let error = load_config_from_str(&updated).expect_err("postgres backend needs db url ref");
    assert!(
        error.to_string().contains("logging.supabase.db_url_ref"),
        "got: {error}",
    );
}

#[test]
fn supabase_postgres_backend_accepts_prefixed_public_tables() {
    let updated = enable_supabase(VALID_CONFIG).replace(
        "[logging.supabase]",
        "[logging.supabase]\nbackend = \"postgres\"\ntable_prefix = \"acp_stack_\"\ndb_url_ref = \"SUPABASE_LOG_DB_URL\"",
    ).replace(r#"schema = "acp_stack""#, r#"schema = "public""#);
    let config = load_config_from_str(&updated).expect("postgres supabase config parses");
    let supabase = config.logging.supabase.expect("supabase set");
    assert_eq!(
        supabase.backend,
        acp_stack::config::SupabaseLoggingBackend::Postgres
    );
    assert_eq!(supabase.table_prefix, "acp_stack_");
    assert_eq!(supabase.db_url_ref.as_deref(), Some("SUPABASE_LOG_DB_URL"));
}

#[test]
fn parses_config_with_explicit_version() {
    let input = format!("config_version = 1\n{VALID_CONFIG}");
    let config = load_config_from_str(&input).expect("explicit version 1 should parse");
    assert_eq!(config.config_version, 1);
}

#[test]
fn accepts_missing_config_version_as_version_1() {
    let config = load_config_from_str(VALID_CONFIG).expect("missing version should parse");
    assert_eq!(config.config_version, 1);
}

#[test]
fn rejects_unsupported_config_version() {
    let input = format!("config_version = 99\n{VALID_CONFIG}");
    let error = load_config_from_str(&input).expect_err("unsupported version should be rejected");
    assert!(
        error.to_string().contains("unsupported config version"),
        "got: {error}"
    );
}

#[test]
fn export_includes_config_version() {
    let config = load_config_from_str(VALID_CONFIG).expect("valid config");
    let canonical = config.to_canonical_toml().expect("canonical");
    assert!(canonical.starts_with("config_version = 1\n"));
    assert_eq!(canonical.matches("config_version = 1").count(), 1);
}

#[test]
fn commands_progress_interval_defaults_and_overrides() {
    let config = load_config_from_str(VALID_CONFIG).expect("default config should parse");
    assert_eq!(
        config.commands.progress_interval,
        DEFAULT_COMMAND_PROGRESS_INTERVAL
    );

    let config_text = format!(
        "{VALID_CONFIG}\n\
         [commands]\n\
         default_timeout = \"10m\"\n\
         cancel_grace = \"5s\"\n\
         progress_interval = \"250ms\"\n\
         env_allowlist = []\n\
         max_output_bytes = 1048576\n"
    );
    let config = load_config_from_str(&config_text).expect("commands override should parse");
    assert_eq!(config.commands.progress_interval, "250ms");
}

#[test]
fn duration_parser_accepts_day_and_week_units() {
    assert_eq!(
        parse_duration_string("1d"),
        Some(std::time::Duration::from_secs(86_400))
    );
    assert_eq!(
        parse_duration_string("3d"),
        Some(std::time::Duration::from_secs(259_200))
    );
    assert_eq!(
        parse_duration_string("4w"),
        Some(std::time::Duration::from_secs(2_419_200))
    );
    assert_eq!(parse_duration_string("0d"), Some(std::time::Duration::ZERO));
    assert!(parse_duration_string("1mo").is_none());
    assert!(parse_duration_string(&format!("{}w", u64::MAX)).is_none());
}

#[test]
fn parses_agent_auto_update_config() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.auto_update]\n\
         enabled = true\n\
         frequency = \"3d\"\n"
    );
    let config = load_config_from_str(&config_text).expect("auto-update config should parse");
    let auto_update = config.agent.auto_update.expect("auto-update configured");
    assert!(auto_update.enabled);
    assert_eq!(auto_update.frequency, "3d");
}

#[test]
fn rejects_agent_auto_update_zero_frequency() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [agent.auto_update]\n\
         enabled = true\n\
         frequency = \"0d\"\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("zero agent.auto_update.frequency must be rejected");
    assert!(
        err.to_string().contains("agent.auto_update.frequency"),
        "got: {err}"
    );
}

#[test]
fn stack_update_config_defaults_to_security_critical_daily() {
    let config = load_config_from_str(VALID_CONFIG).expect("default config should parse");
    assert_eq!(
        config.updates.acp_stack.policy,
        acp_stack::config::StackUpdatePolicy::SecurityCritical
    );
    assert_eq!(config.updates.acp_stack.frequency, "1d");
}

#[test]
fn parses_stack_update_config() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [updates.acp_stack]\n\
         policy = \"compatible\"\n\
         frequency = \"4w\"\n"
    );
    let config = load_config_from_str(&config_text).expect("stack update config should parse");
    assert_eq!(
        config.updates.acp_stack.policy,
        acp_stack::config::StackUpdatePolicy::Compatible
    );
    assert_eq!(config.updates.acp_stack.frequency, "4w");
}

#[test]
fn rejects_stack_update_zero_frequency() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [updates.acp_stack]\n\
         policy = \"security-critical\"\n\
         frequency = \"0d\"\n"
    );
    let err =
        load_config_from_str(&config_text).expect_err("zero stack update frequency is invalid");
    assert!(
        err.to_string().contains("updates.acp_stack.frequency"),
        "got: {err}"
    );
}

#[test]
fn rejects_stack_update_sub_day_frequency() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [updates.acp_stack]\n\
         policy = \"security-critical\"\n\
         frequency = \"12h\"\n"
    );
    let err =
        load_config_from_str(&config_text).expect_err("sub-day stack update frequency is invalid");
    assert!(
        err.to_string().contains("updates.acp_stack.frequency"),
        "got: {err}"
    );
}

#[test]
fn rejects_stack_update_overflowing_frequency() {
    // A day/week count that overflows `Duration` passes the unit check but must
    // still be rejected so config validation matches the runtime parser.
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [updates.acp_stack]\n\
         policy = \"security-critical\"\n\
         frequency = \"99999999999999999w\"\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("an overflowing stack update frequency is invalid");
    assert!(
        err.to_string().contains("updates.acp_stack.frequency"),
        "got: {err}"
    );
}

#[test]
fn rejects_stack_update_frequency_exceeding_epoch() {
    // `9999w` (~192 years) is representable as a `Duration` but longer than the
    // time since 1970-01-01, so it must be rejected by the shared epoch hardstop.
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [updates.acp_stack]\n\
         policy = \"security-critical\"\n\
         frequency = \"9999w\"\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("a stack update frequency longer than the epoch span is invalid");
    assert!(
        err.to_string().contains("updates.acp_stack.frequency"),
        "got: {err}"
    );
}

#[test]
fn rejects_commands_with_invalid_progress_interval() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [commands]\n\
         default_timeout = \"10m\"\n\
         cancel_grace = \"5s\"\n\
         progress_interval = \"0s\"\n\
         env_allowlist = []\n\
         max_output_bytes = 1048576\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("zero commands.progress_interval must be rejected");
    assert!(
        err.to_string().contains("commands.progress_interval"),
        "got: {err}"
    );
}

#[test]
fn parses_prompts_block_with_overrides() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [prompts]\n\
         stale_threshold = \"10m\"\n\
         sweep_interval = \"45s\"\n"
    );
    let config = load_config_from_str(&config_text).expect("config with [prompts] should parse");
    assert_eq!(config.prompts.stale_threshold, "10m");
    assert_eq!(config.prompts.sweep_interval, "45s");
    assert_eq!(
        config.prompts.effective_stale_threshold(),
        std::time::Duration::from_secs(600)
    );
    assert_eq!(
        config.prompts.effective_sweep_interval(),
        std::time::Duration::from_secs(45)
    );
}

#[test]
fn omitted_prompts_block_falls_back_to_defaults() {
    let config = load_config_from_str(VALID_CONFIG).expect("default config should parse");
    assert_eq!(config.prompts.stale_threshold, "5m");
    assert_eq!(config.prompts.sweep_interval, "30s");
    assert_eq!(
        config.prompts.effective_stale_threshold(),
        std::time::Duration::from_secs(300)
    );
    assert_eq!(
        config.prompts.effective_sweep_interval(),
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn rejects_prompts_with_zero_duration() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [prompts]\n\
         stale_threshold = \"0s\"\n\
         sweep_interval = \"30s\"\n"
    );
    let err =
        load_config_from_str(&config_text).expect_err("zero stale_threshold must be rejected");
    assert!(
        err.to_string().contains("prompts.stale_threshold"),
        "got: {err}"
    );
}

#[test]
fn rejects_prompts_with_unparsable_duration() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [prompts]\n\
         stale_threshold = \"not-a-duration\"\n\
         sweep_interval = \"30s\"\n"
    );
    let err =
        load_config_from_str(&config_text).expect_err("garbage stale_threshold must be rejected");
    assert!(
        err.to_string().contains("prompts.stale_threshold"),
        "got: {err}"
    );
}

#[test]
fn rejects_duration_field_exceeding_epoch_floor() {
    // The 1970 hardstop is shared by every duration field, not just the stack
    // frequency: `30000d` (~82 years) exceeds the time since the Unix epoch.
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [prompts]\n\
         stale_threshold = \"30000d\"\n\
         sweep_interval = \"30s\"\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("a stale_threshold longer than the epoch span is invalid");
    assert!(
        err.to_string().contains("prompts.stale_threshold"),
        "got: {err}"
    );
}

#[test]
fn removed_sandbox_network_block_gets_migration_error() {
    // The former `[workspace.sandbox.network]` block moved to the extensions
    // framework; any occurrence must fail fast with a pointer at the new form.
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [workspace.sandbox.network]\n\
         mode = \"isolated\"\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("the removed network block must fail with a migration error");
    assert!(err.to_string().contains("network-provider"), "got: {err}");
    assert!(
        err.to_string().contains("[extensions.<name>]"),
        "got: {err}"
    );
}

#[test]
fn rejects_network_provider_extension_outside_unshare() {
    for mode in ["off", "bwrap"] {
        let config_text = format!(
            "{VALID_CONFIG}\n\
             [workspace.sandbox]\n\
             mode = \"{mode}\"\n\
             [extensions.egress]\n\
             type = \"network-provider\"\n"
        );
        let err = load_config_from_str(&config_text)
            .expect_err("a network-provider extension must require the unshare backend");
        assert!(err.to_string().contains("unshare"), "got: {err}");
        assert!(err.to_string().contains("egress"), "got: {err}");
    }
}

#[test]
fn rejects_multiple_network_provider_extensions() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [extensions.egress-a]\n\
         type = \"network-provider\"\n\
         [extensions.egress-b]\n\
         type = \"network-provider\"\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("more than one network-provider extension must be rejected");
    assert!(err.to_string().contains("at most one"), "got: {err}");
}

#[test]
fn rejects_empty_network_provider_argv_entries() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [extensions.egress]\n\
         type = \"network-provider\"\n\
         provider = [\"/usr/local/libexec/provider\", \" \"]\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("whitespace provider argv entries must be rejected");
    assert!(err.to_string().contains("non-empty"), "got: {err}");
}

#[test]
fn rejects_relative_network_provider_executable() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [extensions.egress]\n\
         type = \"network-provider\"\n\
         provider = [\"acps-network-provider\"]\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("a bare-name provider executable must be rejected");
    assert!(err.to_string().contains("absolute path"), "got: {err}");
}

#[test]
fn rejects_capability_on_network_provider_extension() {
    // A managed-state field on a network-provider instance would look
    // configured while enforcing nothing.
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [extensions.egress]\n\
         type = \"network-provider\"\n\
         capability = \"provider-credential\"\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("capability on a network-provider extension must be rejected");
    assert!(
        err.to_string().contains("managed-state field"),
        "got: {err}"
    );
}

#[test]
fn rejects_provider_fields_on_managed_state_extension() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [extensions.platform-state]\n\
         type = \"managed-state\"\n\
         capability = \"provider-credential\"\n\
         provider = [\"/usr/local/libexec/provider\"]\n"
    );
    let err = load_config_from_str(&config_text)
        .expect_err("provider argv on a managed-state extension must be rejected");
    assert!(
        err.to_string().contains("network-provider field"),
        "got: {err}"
    );
}

#[test]
fn managed_state_extension_requires_known_capability() {
    let missing = format!(
        "{VALID_CONFIG}\n\
         [extensions.platform-state]\n\
         type = \"managed-state\"\n"
    );
    let err = load_config_from_str(&missing)
        .expect_err("managed-state without a capability must be rejected");
    assert!(err.to_string().contains("capability"), "got: {err}");

    let unknown = format!(
        "{VALID_CONFIG}\n\
         [extensions.platform-state]\n\
         type = \"managed-state\"\n\
         capability = \"telemetry\"\n"
    );
    let err = load_config_from_str(&unknown)
        .expect_err("an unknown managed-state capability must be rejected");
    assert!(
        err.to_string().contains("unknown managed-state capability"),
        "got: {err}"
    );
}

#[test]
fn rejects_invalid_extension_names() {
    for name in ["\"UPPER\"", "\"has_underscore\"", "\"trailing-\""] {
        let config_text = format!(
            "{VALID_CONFIG}\n\
             [extensions.{name}]\n\
             type = \"managed-state\"\n\
             capability = \"provider-credential\"\n"
        );
        let err = load_config_from_str(&config_text)
            .expect_err("extension names outside the conservative charset must be rejected");
        assert!(
            err.to_string().contains("lowercase alphanumeric"),
            "name {name} got: {err}"
        );
    }
}

#[test]
fn rejects_zero_network_provider_timeout() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [extensions.egress]\n\
         type = \"network-provider\"\n\
         provider_timeout = \"0s\"\n"
    );
    let err =
        load_config_from_str(&config_text).expect_err("zero provider_timeout must be rejected");
    assert!(err.to_string().contains("greater than zero"), "got: {err}");
}

#[test]
fn rejects_invalid_network_provider_timeout() {
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [extensions.egress]\n\
         type = \"network-provider\"\n\
         provider_timeout = \"soon\"\n"
    );
    let err =
        load_config_from_str(&config_text).expect_err("garbage provider_timeout must be rejected");
    assert!(
        err.to_string().contains("extensions.provider_timeout"),
        "got: {err}"
    );
}

#[test]
fn network_provider_extension_round_trips_and_defaults() {
    // Empty provider is legal (deny-all networking); timeout falls back to 30s.
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [extensions.egress]\n\
         type = \"network-provider\"\n"
    );
    let config = load_config_from_str(&config_text).expect("network-provider extension parses");
    let network = acp_stack::extensions::resolve_network_provider(&config)
        .expect("declared network-provider resolves");
    assert_eq!(network.name, "egress");
    assert!(network.provider.is_empty());
    assert_eq!(network.provider_timeout_raw(), "30s");

    let canonical = config.to_canonical_toml().expect("canonical export");
    assert!(
        canonical.contains("[extensions.egress]"),
        "extensions must round-trip through canonical TOML, got:\n{canonical}"
    );
    let reparsed = load_config_from_str(&canonical).expect("canonical extension config parses");
    assert_eq!(reparsed.extensions, config.extensions);
}

#[test]
fn network_provider_and_managed_state_extensions_coexist() {
    // `resolve_network_provider` filters by type; a managed-state sibling
    // must not affect resolution and both must survive the round-trip.
    let config_text = format!(
        "{VALID_CONFIG}\n\
         [workspace.sandbox]\n\
         mode = \"unshare\"\n\
         [extensions.egress]\n\
         type = \"network-provider\"\n\
         [extensions.platform-state]\n\
         type = \"managed-state\"\n\
         capability = \"provider-credential\"\n"
    );
    let config = load_config_from_str(&config_text).expect("both extension types parse");
    let network = acp_stack::extensions::resolve_network_provider(&config)
        .expect("network-provider resolves next to a managed-state sibling");
    assert_eq!(network.name, "egress");
    let canonical = config.to_canonical_toml().expect("canonical export");
    let reparsed = load_config_from_str(&canonical).expect("canonical config parses");
    assert_eq!(reparsed.extensions.len(), 2);
}

#[test]
fn absent_extensions_serialize_to_absent_table() {
    let config = load_config_from_str(VALID_CONFIG).expect("base config parses");
    assert!(config.extensions.is_empty());
    let canonical = config.to_canonical_toml().expect("canonical export");
    assert!(
        !canonical.contains("[extensions"),
        "an empty extensions table must round-trip to an absent table, got:\n{canonical}"
    );
}
