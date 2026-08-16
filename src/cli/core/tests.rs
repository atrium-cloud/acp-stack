use super::{Cli, Command, daemon_base_url, static_path_label, strip_ansi};
use clap::Parser;

#[test]
fn strip_ansi_removes_csi_sequences() {
    assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    assert_eq!(
        strip_ansi("plain \x1b[1;33mhighlight\x1b[0m end"),
        "plain highlight end"
    );
}

#[test]
fn strip_ansi_passes_plain_text_unchanged() {
    assert_eq!(strip_ansi("nothing to strip"), "nothing to strip");
}

#[test]
fn strip_ansi_preserves_other_control_characters() {
    // Tabs, newlines, and other control chars survive: serde_json escapes them downstream.
    assert_eq!(strip_ansi("a\tb\nc"), "a\tb\nc");
}

#[test]
fn daemon_base_url_prefers_public_url() {
    assert_eq!(
        daemon_base_url(Some("https://agent.example.com/root"), "0.0.0.0:7700").expect("url"),
        "https://agent.example.com/root"
    );
}

#[test]
fn daemon_base_url_rewrites_wildcard_binds_to_loopback() {
    assert_eq!(
        daemon_base_url(None, "0.0.0.0:7700").expect("url"),
        "http://127.0.0.1:7700"
    );
    assert_eq!(
        daemon_base_url(None, "[::]:7700").expect("url"),
        "http://[::1]:7700"
    );
}

#[test]
fn daemon_base_url_preserves_explicit_loopback_bind() {
    assert_eq!(
        daemon_base_url(None, "127.0.0.1:7700").expect("url"),
        "http://127.0.0.1:7700"
    );
}

#[test]
fn static_path_label_covers_cli_daemon_routes() {
    let cases = [
        ("/v1/metrics/summary?range=day", "/v1/metrics/summary"),
        ("/v1/logs/events?limit=10", "/v1/logs/events"),
        ("/v1/files/content?path=README.md", "/v1/files/content"),
        ("/v1/commands/cmd_123/output", "/v1/commands/{id}/output"),
        (
            "/v1/permissions/pending?limit=10",
            "/v1/permissions/pending",
        ),
        (
            "/v1/sessions/sess_123/snapshot",
            "/v1/sessions/{id}/snapshot",
        ),
        (
            "/v1/auth/local-session-access",
            "/v1/auth/local-session-access",
        ),
        ("/v1/agent/restart", "/v1/agent/restart"),
        ("/v1/agent/restart-blockers", "/v1/agent/restart-blockers"),
        ("/v1/agent/skills", "/v1/agent/skills"),
        ("/v1/agent/skills/catalog", "/v1/agent/skills/catalog"),
        ("/v1/agent/skills/add", "/v1/agent/skills/add"),
        ("/v1/agent/skills/remove", "/v1/agent/skills/remove"),
        ("/v1/agent/skills/source", "/v1/agent/skills/source"),
        (
            "/v1/agent/skills/sources/add",
            "/v1/agent/skills/sources/add",
        ),
        (
            "/v1/agent/skills/sources/remove",
            "/v1/agent/skills/sources/remove",
        ),
        (
            "/v1/agent/config/native/inspect",
            "/v1/agent/config/native/inspect",
        ),
        (
            "/v1/agent/config/native/import",
            "/v1/agent/config/native/import",
        ),
        (
            "/v1/agent/config/native/import/nci_123",
            "/v1/agent/config/native/import/{operation_id}",
        ),
        (
            "/v1/agent/config/native/import/nci_123/cancel",
            "/v1/agent/config/native/import/{operation_id}/cancel",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(static_path_label(input), expected);
    }
}

#[test]
fn cli_parses_top_level_restart_auto() {
    let cli = Cli::try_parse_from(["acps", "restart", "auto"]).expect("restart auto parses");
    match cli.command {
        Command::Restart(args) => {
            assert!(args.command.is_some());
        }
        other => panic!("expected restart command, got {other:?}"),
    }
}

#[test]
fn cli_parses_agent_and_array_restart_auto() {
    Cli::try_parse_from(["acps", "agent", "restart", "auto"]).expect("agent restart auto parses");
    Cli::try_parse_from(["acps", "array", "restart", "auto"]).expect("array restart auto parses");
    Cli::try_parse_from(["acps", "array", "restart", "auto", "--target", "codex"])
        .expect("array restart auto target parses");
    Cli::try_parse_from(["acps", "array", "restart", "--target", "codex", "auto"])
        .expect("array parent target restart auto parses");
}

#[test]
fn cli_parses_native_agent_config_commands() {
    Cli::try_parse_from(["acps", "agent", "config", "inspect", "opencode.json"])
        .expect("native config inspect parses");
    Cli::try_parse_from([
        "acps",
        "agent",
        "config",
        "import",
        "opencode.json",
        "--managed-field",
        "provider",
        "--managed-field",
        "model",
        "--ack-executable-settings",
    ])
    .expect("native config import parses");
}

#[test]
fn cli_parses_provider_catalog_and_active_set_commands() {
    for args in [
        vec!["acps", "agent", "provider", "use", "opencode-go"],
        vec![
            "acps",
            "agent",
            "provider",
            "set-active",
            "opencode-go,openrouter",
        ],
        vec!["acps", "agent", "provider", "list-active"],
        vec![
            "acps",
            "agent",
            "provider",
            "credential",
            "add",
            "opencode-go",
            "--existing-alias",
            "go_1",
            "--alias",
            "go_2",
            "--from-secret",
            "OPENCODE_API_KEY=GO_KEY_2",
        ],
        vec![
            "acps",
            "agent",
            "provider",
            "credential",
            "update",
            "opencode-go",
            "go_2",
            "--from-secret",
            "OPENCODE_API_KEY=GO_KEY_3",
        ],
        vec![
            "acps",
            "agent",
            "provider",
            "credential",
            "select",
            "opencode-go",
            "go_2",
        ],
        vec![
            "acps",
            "array",
            "provider",
            "use",
            "--target",
            "worker",
            "openrouter",
        ],
        vec![
            "acps",
            "array",
            "provider",
            "credential",
            "select",
            "--target",
            "worker",
            "opencode-go",
            "go_2",
        ],
    ] {
        Cli::try_parse_from(args).expect("provider command parses");
    }
}

#[test]
fn cli_parses_skills_commands() {
    for args in [
        vec!["acps", "skills", "list"],
        vec!["acps", "skills", "list", "--format", "json"],
        vec!["acps", "skills", "catalog"],
        vec!["acps", "skills", "add", "anthropic", "docx", "pptx"],
        vec!["acps", "skills", "add", "github:my-org", "my-skill"],
        vec!["acps", "skills", "remove", "docx"],
        vec!["acps", "skills", "remove", "zoom/android"],
        vec!["acps", "skills", "source", "get", "anthropic"],
        vec!["acps", "skills", "source", "get", "github:my-org/skills"],
        vec![
            "acps",
            "skills",
            "source",
            "add",
            "my-org",
            "my-org/skills",
            "--trusted",
        ],
        vec![
            "acps",
            "skills",
            "source",
            "add",
            "my-org",
            "my-org/skills",
            "--branch",
            "dev",
        ],
        vec!["acps", "skills", "source", "remove", "my-org"],
    ] {
        Cli::try_parse_from(args).expect("skills command parses");
    }
}

#[test]
fn cli_skills_add_requires_at_least_one_skill() {
    Cli::try_parse_from(["acps", "skills", "add", "anthropic"])
        .expect_err("add requires a skill selector");
}

#[test]
fn cli_parses_workspace_commands() {
    for args in [
        vec!["acps", "workspace", "status"],
        vec!["acps", "workspace", "sync"],
        vec!["acps", "workspace", "code-source", "list"],
        vec![
            "acps",
            "workspace",
            "code-source",
            "add",
            "--repo",
            "https://github.com/example/app.git",
            "--no-sync",
        ],
        vec![
            "acps",
            "workspace",
            "data-source",
            "add",
            "--type",
            "local",
            "--path",
            "/data/input",
            "--no-sync",
        ],
        vec![
            "acps",
            "workspace",
            "sandbox",
            "set",
            "--mode",
            "custom",
            "--wrapper-arg",
            "systemd-run",
        ],
    ] {
        Cli::try_parse_from(args).expect("workspace command parses");
    }
}
