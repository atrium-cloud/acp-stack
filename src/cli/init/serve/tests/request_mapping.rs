//! Request DTO, validation, and arg-mapping tests for the hosted init boundary.

use super::super::*;
use super::support::*;

#[test]
fn start_init_request_maps_sandbox_into_args() {
    // `deny_unknown_fields` means the platform payload is rejected outright
    // unless `sandbox` is a known field; this also covers the arg mapping.
    let request: StartInitRequest =
        serde_json::from_str(r#"{"agent":"placebo","sandbox":"unshare"}"#)
            .expect("sandbox must be an accepted request field");
    let args = request.into_init_args().expect("valid request");
    assert_eq!(args.sandbox.as_deref(), Some("unshare"));
    // The wire spells the repeatable custom-agent argument list in the
    // plural; the CLI's singular `--custom-agent-arg` is not a field name,
    // and `deny_unknown_fields` has to keep saying so.
    assert!(
        serde_json::from_str::<StartInitRequest>(r#"{"custom_agent_args":["--stdio"]}"#).is_ok(),
        "custom_agent_args must be an accepted request field"
    );
    assert!(
        serde_json::from_str::<StartInitRequest>(r#"{"custom_agent_arg":["--stdio"]}"#).is_err(),
        "the singular CLI flag spelling must not be accepted"
    );
}

#[test]
fn start_init_request_maps_custom_agent_declaration_into_args() {
    let args = request_from_json(
        r#"{
                "custom_agent_id": "housebot",
                "custom_agent_name": "House Bot",
                "custom_agent_command": "housebot-acp",
                "custom_agent_args": ["--stdio", "--quiet"],
                "custom_agent_install": "npm install -g housebot",
                "custom_agent_creates": "/usr/local/bin/housebot-acp"
            }"#,
    )
    .into_init_args()
    .expect("valid request");
    assert_eq!(args.custom_agent_id.as_deref(), Some("housebot"));
    assert_eq!(args.custom_agent_name.as_deref(), Some("House Bot"));
    assert_eq!(args.custom_agent_command.as_deref(), Some("housebot-acp"));
    assert_eq!(
        args.custom_agent_arg,
        vec!["--stdio".to_owned(), "--quiet".to_owned()]
    );
    assert_eq!(
        args.custom_agent_install.as_deref(),
        Some("npm install -g housebot")
    );
    assert_eq!(
        args.custom_agent_creates.as_deref(),
        Some("/usr/local/bin/housebot-acp")
    );
    assert!(args.agent.is_none());
    // The spec assembles through the same resolver the CLI flags use.
    let spec = super::super::super::registry_apply::resolve_custom_agent_spec(&args)
        .expect("spec must resolve")
        .expect("a declared custom agent must produce a spec");
    assert_eq!(spec.id, "housebot");
    assert_eq!(spec.creates, "/usr/local/bin/housebot-acp");
}

#[test]
fn start_init_request_rejects_custom_agent_fields_without_id() {
    // Mirrors clap's `requires = "custom_agent_id"` on each dependent flag.
    for (field, payload) in [
        ("custom_agent_name", r#"{"custom_agent_name": "House Bot"}"#),
        (
            "custom_agent_command",
            r#"{"custom_agent_command": "housebot-acp"}"#,
        ),
        ("custom_agent_args", r#"{"custom_agent_args": ["--stdio"]}"#),
        (
            "custom_agent_install",
            r#"{"custom_agent_install": "npm install -g housebot"}"#,
        ),
        (
            "custom_agent_creates",
            r#"{"custom_agent_creates": "/usr/local/bin/housebot-acp"}"#,
        ),
    ] {
        let error = request_from_json(payload)
            .into_init_args()
            .expect_err("a dependent custom-agent field needs custom_agent_id");
        match error {
            StackError::InvalidParam {
                field: rejected,
                ref reason,
            } => {
                assert_eq!(rejected, field);
                // The rejection names fields, never the submitted spec.
                for value in ["House Bot", "housebot-acp", "--stdio", "npm install"] {
                    assert!(!reason.contains(value), "{reason} echoed a submitted value");
                }
            }
            other => panic!("expected an InvalidParam for {field}, got {other}"),
        }
    }
}

#[test]
fn start_init_request_rejects_custom_agent_conflicts_without_echoing_values() {
    for payload in [
        r#"{"custom_agent_id": "housebot", "agent": "opencode"}"#,
        r#"{"custom_agent_id": "housebot", "provider": "openrouter"}"#,
        r#"{"custom_agent_id": "housebot", "model": "openai/gpt-5"}"#,
        r#"{"custom_agent_id": "housebot", "mode": "plan"}"#,
        r#"{"custom_agent_id": "housebot", "custom_provider": true}"#,
    ] {
        let error = request_from_json(payload)
            .into_init_args()
            .expect_err("registry knobs conflict with a custom agent");
        match error {
            StackError::InvalidParam { field, ref reason } => {
                assert_eq!(field, "custom_agent_id");
                for value in ["housebot", "opencode", "openrouter", "openai/gpt-5", "plan"] {
                    assert!(!reason.contains(value), "{reason} echoed a submitted value");
                }
            }
            other => panic!("expected an InvalidParam, got {other}"),
        }
    }
    // An explicitly false boolean declares nothing, so it must not collide.
    let benign = request_from_json(
        r#"{"custom_agent_id": "housebot", "custom_provider": false,
                "custom_agent_command": "housebot-acp",
                "custom_agent_install": "npm install -g housebot"}"#,
    )
    .into_init_args()
    .expect("custom_provider:false is not a declaration");
    assert!(!benign.custom_provider);
}

#[test]
fn start_init_request_rejects_a_custom_agent_that_cannot_launch_or_install() {
    // The resolver treats both as mandatory. Rejecting here instead of at
    // the resolver keeps an incomplete spec from consuming the session slot
    // and parking it errored for the ack grace.
    for (field, payload) in [
        ("custom_agent_command", r#"{"custom_agent_id": "housebot"}"#),
        (
            "custom_agent_command",
            r#"{"custom_agent_id": "housebot", "custom_agent_install": "npm i -g housebot"}"#,
        ),
        (
            "custom_agent_install",
            r#"{"custom_agent_id": "housebot", "custom_agent_command": "housebot-acp"}"#,
        ),
        // Blank is absent, exactly as `require_custom_flag` reads it.
        (
            "custom_agent_command",
            r#"{"custom_agent_id": "housebot", "custom_agent_command": "  ",
                    "custom_agent_install": "npm i -g housebot"}"#,
        ),
        (
            "custom_agent_install",
            r#"{"custom_agent_id": "housebot", "custom_agent_command": "housebot-acp",
                    "custom_agent_install": ""}"#,
        ),
    ] {
        let error = request_from_json(payload)
            .into_init_args()
            .expect_err("an incomplete custom-agent spec must be rejected at the boundary");
        match error {
            StackError::InvalidParam {
                field: rejected,
                ref reason,
            } => {
                assert_eq!(rejected, field);
                // Request-field terms, never the CLI flag spelling, and
                // never the submitted spec.
                assert!(!reason.contains("--"), "{reason} names a CLI flag");
                for value in ["housebot", "npm i -g"] {
                    assert!(!reason.contains(value), "{reason} echoed a submitted value");
                }
            }
            other => panic!("expected an InvalidParam for {field}, got {other}"),
        }
    }
}

#[test]
fn start_init_request_maps_mode_into_args() {
    // Declare-up-front parity with provider/model: a hosted client that
    // knows its mode must not have to answer the streamed picker.
    let args = request_from_json(r#"{"agent": "opencode", "mode": "plan"}"#)
        .into_init_args()
        .expect("valid request");
    assert_eq!(args.mode.as_deref(), Some("plan"));

    let absent = request_from_json(r#"{"agent": "opencode"}"#)
        .into_init_args()
        .expect("valid request");
    assert_eq!(absent.mode, None);
}

#[test]
fn start_init_request_maps_resume_and_fresh() {
    let resume = request_from_json(r#"{"resume": true}"#)
        .into_init_args()
        .expect("valid request");
    assert!(resume.resume);
    assert!(!resume.fresh);
    // A resume that redeclares no skills must not inherit the hosted
    // `no_skills` default, or the recorded skill plan replay is skipped.
    assert!(!resume.no_skills);

    let fresh = request_from_json(r#"{"fresh": true}"#)
        .into_init_args()
        .expect("valid request");
    assert!(fresh.fresh);
    assert!(!fresh.resume);
    assert!(fresh.no_skills);

    let conflict = request_from_json(r#"{"resume": true, "fresh": true}"#)
        .into_init_args()
        .expect_err("resume and fresh are mutually exclusive");
    assert!(matches!(
        conflict,
        StackError::InvalidParam {
            field: "resume",
            ..
        }
    ));

    // Requests that say nothing keep the pre-existing defaults.
    let quiet = request_from_json(r#"{}"#)
        .into_init_args()
        .expect("valid request");
    assert!(!quiet.resume);
    assert!(!quiet.fresh);
    assert!(quiet.custom_agent_id.is_none());
    assert!(quiet.custom_agent_arg.is_empty());
}

#[test]
fn hosted_resume_reuses_the_recorded_run_and_its_rotation() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store =
        crate::state::StateStore::open(tempdir.path().join("state.sqlite")).expect("state store");
    store.migrate().expect("migrate");

    // The run a crashed hosted session left behind: `run_init_with_output`
    // folds the forced rotation into the flag before the row is recorded.
    let mut first = request_from_json(r#"{"agent": "opencode"}"#)
        .into_init_args()
        .expect("valid request");
    first.rotate_keys = true;
    let recorded_run = super::super::super::resume::resolve_init_run(&first, &store)
        .expect("record the first run");

    let resumed_args = request_from_json(r#"{"resume": true}"#)
        .into_init_args()
        .expect("valid request");
    let resumed_run = super::super::super::resume::resolve_init_run(&resumed_args, &store)
        .expect("hosted resume must adopt the recorded run");
    assert_eq!(resumed_run.id, recorded_run.id);

    // Rotation has no request field; it rides the recorded args, so a
    // hosted resume rotates exactly like a CLI `--resume` of the same run.
    let recorded = super::super::super::resume::recorded_init_args(&resumed_run).expect("recorded");
    assert!(recorded.rotate_keys);
    assert_eq!(recorded.agent.as_deref(), Some("opencode"));
}

#[test]
fn start_init_request_maps_mcp_declarations_into_prompt_fields() {
    let args = request_from_json(
            r#"{
                "mcp_preset": ["linear"],
                "mcp_stdio": [
                    {"name": "files", "command": "mcp-files", "args": ["--root", "/data"], "env": ["FILES_TOKEN"]}
                ],
                "mcp_http": [
                    {"name": "search", "url": "https://mcp.example.com/mcp",
                     "headers": [{"name": "Authorization", "value_ref": "SEARCH_API_KEY"}]}
                ]
            }"#,
        )
        .into_init_args()
        .expect("valid request");
    assert_eq!(args.mcp_preset, vec!["linear".to_owned()]);
    assert!(args.mcp_stdio.is_empty());
    assert!(args.mcp_http.is_empty());
    assert_eq!(args.prompt_mcp_stdio.len(), 1);
    let stdio = &args.prompt_mcp_stdio[0];
    assert_eq!(stdio.name, "files");
    assert_eq!(stdio.command, "mcp-files");
    assert_eq!(stdio.args, vec!["--root".to_owned(), "/data".to_owned()]);
    assert_eq!(stdio.env, vec!["FILES_TOKEN".to_owned()]);
    assert_eq!(args.prompt_mcp_http.len(), 1);
    let http = &args.prompt_mcp_http[0];
    assert_eq!(http.name, "search");
    assert_eq!(http.url, "https://mcp.example.com/mcp");
    assert_eq!(http.headers.len(), 1);
    assert_eq!(http.headers[0].name, "Authorization");
    assert_eq!(http.headers[0].value_ref.as_deref(), Some("SEARCH_API_KEY"));
    assert_eq!(http.headers[0].value, None);
}

#[test]
fn start_init_request_accepts_templated_header_and_env() {
    let args = request_from_json(
        r#"{
                "mcp_stdio": [
                    {"name": "db", "command": "db-mcp", "env": ["API_KEY", "URL=x-${DB_PASS}"]}
                ],
                "mcp_http": [
                    {"name": "relay", "url": "http://127.0.0.1:8787/mcp",
                     "headers": [{"name": "Authorization", "value": "Bearer ${RELAY_TOKEN}"}]}
                ]
            }"#,
    )
    .into_init_args()
    .expect("valid request");
    assert_eq!(
        args.prompt_mcp_stdio[0].env,
        vec!["API_KEY".to_owned(), "URL=x-${DB_PASS}".to_owned()]
    );
    let header = &args.prompt_mcp_http[0].headers[0];
    assert_eq!(header.value.as_deref(), Some("Bearer ${RELAY_TOKEN}"));
    assert_eq!(header.value_ref, None);
}

#[test]
fn start_init_request_rejects_header_with_both_or_neither_value_source() {
    let both = request_from_json(
        r#"{"mcp_http": [{"name": "s", "url": "https://x.example/mcp",
                "headers": [{"name": "A", "value_ref": "R", "value": "${R}"}]}]}"#,
    )
    .into_init_args()
    .expect_err("both set must be rejected");
    assert!(both.to_string().contains("exactly one"), "{both}");

    let neither = request_from_json(
        r#"{"mcp_http": [{"name": "s", "url": "https://x.example/mcp",
                "headers": [{"name": "A"}]}]}"#,
    )
    .into_init_args()
    .expect_err("neither set must be rejected");
    assert!(neither.to_string().contains("exactly one"), "{neither}");
}

#[test]
fn boundary_rejection_of_pasted_credentials_never_echoes_them() {
    let secret = "sk-live-AAAABBBBCCCC";
    let in_template = request_from_json(&format!(
        r#"{{"mcp_http": [{{"name": "s", "url": "https://x.example/mcp",
                "headers": [{{"name": "A", "value": "Bearer ${{{secret}}}"}}]}}]}}"#,
    ))
    .into_init_args()
    .expect_err("secret-shaped template ref must be rejected");
    assert!(!in_template.to_string().contains(secret), "{in_template}");

    let as_value_ref = request_from_json(&format!(
        r#"{{"mcp_http": [{{"name": "s", "url": "https://x.example/mcp",
                "headers": [{{"name": "A", "value_ref": "{secret}"}}]}}]}}"#,
    ))
    .into_init_args()
    .expect_err("secret-shaped value_ref must be rejected");
    assert!(!as_value_ref.to_string().contains(secret), "{as_value_ref}");

    let in_env = request_from_json(&format!(
        r#"{{"mcp_stdio": [{{"name": "db", "command": "db-mcp", "env": ["{secret}"]}}]}}"#,
    ))
    .into_init_args()
    .expect_err("secret-shaped env entry must be rejected");
    assert!(!in_env.to_string().contains(secret), "{in_env}");
}

#[test]
fn start_init_request_rejects_malformed_templates_at_the_boundary() {
    let bad_header = request_from_json(
        r#"{"mcp_http": [{"name": "s", "url": "https://x.example/mcp",
                "headers": [{"name": "A", "value": "Bearer ${unclosed"}]}]}"#,
    )
    .into_init_args()
    .expect_err("unterminated template must be rejected");
    assert!(
        bad_header.to_string().contains("unterminated"),
        "{bad_header}"
    );

    let bad_env = request_from_json(
        r#"{"mcp_stdio": [{"name": "db", "command": "db-mcp", "env": ["URL=plaintext"]}]}"#,
    )
    .into_init_args()
    .expect_err("pure-literal env template must be rejected");
    assert!(
        bad_env.to_string().contains("no `${NAME}` reference"),
        "{bad_env}"
    );
}

#[test]
fn start_init_request_maps_deps_and_flags() {
    let args = request_from_json(
        r#"{
                "deps": [{"name": "ripgrep", "shell": "apt-get install -y ripgrep"}],
                "deps_system": [{"name": "ffmpeg", "shell": "apt-get install -y ffmpeg"}],
                "deps_apply": true,
                "deps_apply_yes": true,
                "standard_agent_work_deps": true,
                "browser_use": true
            }"#,
    )
    .into_init_args()
    .expect("valid request");
    assert_eq!(
        args.dep,
        vec!["ripgrep=apt-get install -y ripgrep".to_owned()]
    );
    assert_eq!(
        args.dep_system,
        vec!["ffmpeg=apt-get install -y ffmpeg".to_owned()]
    );
    assert!(args.deps_apply);
    assert!(args.deps_apply_yes);
    assert!(args.standard_agent_work_deps);
    assert!(args.browser_use_profile);
}

#[test]
fn start_init_request_maps_update_policies_into_args() {
    // Parity with the CLI's `--stack-update`/`--agent-update` flags: the
    // hosted contract must carry both update policies so a non-interactive
    // init can disable them, not just the interactive wizard.
    let args = request_from_json(
        r#"{
                "stack_update": "security",
                "stack_update_frequency": "2w",
                "agent_update": "off"
            }"#,
    )
    .into_init_args()
    .expect("valid request");
    assert_eq!(args.stack_update.as_deref(), Some("security"));
    assert_eq!(args.stack_update_frequency.as_deref(), Some("2w"));
    assert_eq!(args.agent_update.as_deref(), Some("off"));
    assert_eq!(args.agent_update_frequency, None);
}

#[test]
fn start_init_request_rejects_frequency_without_policy() {
    // Mirrors clap's `requires`: a frequency with no policy is a 400 at the
    // boundary rather than a silently dropped field.
    let stack_error = request_from_json(r#"{"stack_update_frequency": "1w"}"#)
        .into_init_args()
        .expect_err("frequency without policy must be rejected");
    assert!(matches!(
        stack_error,
        StackError::InvalidParam {
            field: "stack_update_frequency",
            ..
        }
    ));
    let agent_error = request_from_json(r#"{"agent_update_frequency": "12h"}"#)
        .into_init_args()
        .expect_err("frequency without policy must be rejected");
    assert!(matches!(
        agent_error,
        StackError::InvalidParam {
            field: "agent_update_frequency",
            ..
        }
    ));
}

// Mirrors clap's `requires` on the provider family. Provider processing
// returns early with no provider in hand, so each of these would otherwise
// be accepted and then dropped without a word.
#[test]
fn start_init_request_rejects_provider_fields_without_their_anchor() {
    for (payload, offending) in [
        (r#"{"api_key_ref": "OPENROUTER_API_KEY"}"#, "api_key_ref"),
        (r#"{"custom_provider": true}"#, "custom_provider"),
        (r#"{"provider_name": "House LLM"}"#, "provider_name"),
        (
            r#"{"provider": "house", "base_url": "https://api.house.dev/v1"}"#,
            "base_url",
        ),
        (
            r#"{"provider": "house", "provider_api": "chat-completions"}"#,
            "provider_api",
        ),
        (
            r#"{"provider": "house", "model_name": "House 1"}"#,
            "model_name",
        ),
        (r#"{"provider": "house", "context": "200000"}"#, "context"),
        (
            r#"{"provider": "house", "output_max_tokens": "8192"}"#,
            "output_max_tokens",
        ),
    ] {
        let error = request_from_json(payload)
            .into_init_args()
            .expect_err("an unanchored provider field must be rejected");
        match error {
            StackError::InvalidParam { field, reason } => {
                assert_eq!(field, offending, "payload {payload}");
                assert!(
                    !reason.contains("house") && !reason.contains("House"),
                    "the rejection must not echo submitted values: {reason}"
                );
            }
            other => panic!("expected an InvalidParam for {offending}, got {other:?}"),
        }
    }
}

#[test]
fn start_init_request_accepts_a_fully_anchored_provider_family() {
    let plain =
        request_from_json(r#"{"provider": "openrouter", "api_key_ref": "OPENROUTER_API_KEY"}"#)
            .into_init_args()
            .expect("a provider with its key ref is a complete declaration");
    assert_eq!(plain.provider.as_deref(), Some("openrouter"));
    assert_eq!(plain.api_key_ref.as_deref(), Some("OPENROUTER_API_KEY"));

    let custom = request_from_json(
        r#"{
                "provider": "house",
                "custom_provider": true,
                "provider_name": "House LLM",
                "base_url": "https://api.house.dev/v1",
                "provider_api": "chat-completions",
                "model_name": "House 1",
                "context": "200000",
                "output_max_tokens": "8192"
            }"#,
    )
    .into_init_args()
    .expect("the whole custom-provider family is a complete declaration");
    assert!(custom.custom_provider);
    assert_eq!(custom.provider_name.as_deref(), Some("House LLM"));
    assert_eq!(custom.output_max_tokens.as_deref(), Some("8192"));
}

#[test]
fn start_init_request_skills_declaration_clears_no_skills() {
    let args =
        request_from_json(r#"{"skills_source": "github:example", "skills": ["writing-plans"]}"#)
            .into_init_args()
            .expect("valid request");
    assert!(!args.no_skills);
    assert_eq!(args.skills_source.as_deref(), Some("github:example"));
    assert_eq!(args.skills, vec!["writing-plans".to_owned()]);

    let essential = request_from_json(r#"{"essential_skills": true}"#)
        .into_init_args()
        .expect("valid request");
    assert!(!essential.no_skills);
    assert!(essential.essential_skills);

    let none = request_from_json(r#"{}"#)
        .into_init_args()
        .expect("valid request");
    assert!(none.no_skills);
}

#[test]
fn start_init_request_maps_data_sources() {
    let args = request_from_json(
            r#"{
                "data_sources": [
                    {"type": "local", "path": "/srv/import"},
                    {"type": "https", "url": "https://example.com/data.tar.gz", "expected_sha256": "ab"},
                    {"type": "s3", "name": "corpus", "bucket": "my-bucket", "region": "us-east-1",
                     "prefix": "corpus/", "access_key_ref": "AWS_ACCESS_KEY_ID",
                     "secret_key_ref": "AWS_SECRET_ACCESS_KEY"}
                ]
            }"#,
        )
        .into_init_args()
        .expect("valid request");
    assert_eq!(args.prompt_data_sources.len(), 3);
    assert_eq!(args.prompt_data_sources[0].source_type, "local");
    assert_eq!(
        args.prompt_data_sources[0].path.as_deref(),
        Some("/srv/import")
    );
    assert_eq!(args.prompt_data_sources[1].source_type, "https");
    assert_eq!(
        args.prompt_data_sources[1].url.as_deref(),
        Some("https://example.com/data.tar.gz")
    );
    assert_eq!(
        args.prompt_data_sources[1].expected_sha256.as_deref(),
        Some("ab")
    );
    let s3 = &args.prompt_data_sources[2];
    assert_eq!(s3.source_type, "s3");
    assert_eq!(s3.name.as_deref(), Some("corpus"));
    assert_eq!(s3.bucket.as_deref(), Some("my-bucket"));
    assert_eq!(s3.region.as_deref(), Some("us-east-1"));
    assert_eq!(s3.prefix.as_deref(), Some("corpus/"));
    assert_eq!(s3.access_key_ref.as_deref(), Some("AWS_ACCESS_KEY_ID"));
    assert_eq!(s3.secret_key_ref.as_deref(), Some("AWS_SECRET_ACCESS_KEY"));
}

#[test]
fn start_init_request_rejects_invalid_environment_declarations() {
    assert!(
        serde_json::from_str::<StartInitRequest>(r#"{"mcp_servers": []}"#).is_err(),
        "unknown fields must be rejected"
    );
    assert!(
            serde_json::from_str::<StartInitRequest>(
                r#"{"data_sources": [{"type": "s3", "bucket": "b", "path": "/x", "access_key_ref": "A", "secret_key_ref": "S"}]}"#
            )
            .is_err(),
            "fields from another data-source type must be rejected"
        );
    assert!(
            serde_json::from_str::<StartInitRequest>(
                r#"{"data_sources": [{"type": "s3", "bucket": "b", "access_key_ref": "A", "secret_key_ref": "S"}]}"#
            )
            .is_err(),
            "s3 sources must declare a region"
        );
    for payload in [r#"{"deps_apply_yes": true}"#, r#"{"deps_apply": true}"#] {
        let mismatched_apply = request_from_json(payload).into_init_args();
        assert!(matches!(
            mismatched_apply,
            Err(StackError::InvalidParam {
                field: "deps_apply",
                ..
            })
        ));
    }
    for payload in [
        r#"{"skills_source": "github:example"}"#,
        r#"{"skills": ["writing-plans"]}"#,
    ] {
        let unpaired_skills = request_from_json(payload).into_init_args();
        assert!(matches!(
            unpaired_skills,
            Err(StackError::InvalidParam {
                field: "skills",
                ..
            })
        ));
    }
    let essential_conflict = request_from_json(
        r#"{"essential_skills": true, "skills_source": "github:example", "skills": ["x"]}"#,
    )
    .into_init_args();
    assert!(matches!(
        essential_conflict,
        Err(StackError::InvalidParam {
            field: "essential_skills",
            ..
        })
    ));
    let bad_dep_name =
        request_from_json(r#"{"deps": [{"name": "a=b", "shell": "true"}]}"#).into_init_args();
    assert!(matches!(
        bad_dep_name,
        Err(StackError::InvalidParam { field: "deps", .. })
    ));
    let empty_dep_shell =
        request_from_json(r#"{"deps": [{"name": "a", "shell": " "}]}"#).into_init_args();
    assert!(matches!(
        empty_dep_shell,
        Err(StackError::InvalidParam { field: "deps", .. })
    ));
}

#[test]
fn start_init_request_declarations_assemble_into_starter_config() {
    let args = request_from_json(
            r#"{
                "mcp_stdio": [
                    {"name": "files", "command": "mcp-files", "args": ["--root", "/data"], "env": ["FILES_TOKEN"]}
                ],
                "mcp_http": [
                    {"name": "search", "url": "https://mcp.example.com/mcp",
                     "headers": [{"name": "Authorization", "value_ref": "SEARCH_API_KEY"}]}
                ],
                "deps": [{"name": "ripgrep", "shell": "apt-get install -y ripgrep"}],
                "data_sources": [
                    {"type": "s3", "bucket": "my-bucket", "region": "us-east-1",
                     "access_key_ref": "AWS_ACCESS_KEY_ID", "secret_key_ref": "AWS_SECRET_ACCESS_KEY"}
                ]
            }"#,
        )
        .into_init_args()
        .expect("valid request");
    let toml = super::super::super::starter_config::starter_config(&args)
        .expect("declarations must assemble into a starter config");
    for expected in [
        "name = \"files\"",
        "command = \"mcp-files\"",
        "FILES_TOKEN",
        "https://mcp.example.com/mcp",
        "SEARCH_API_KEY",
        "my-bucket",
        "AWS_SECRET_ACCESS_KEY",
    ] {
        assert!(
            toml.contains(expected),
            "starter config must contain {expected}: {toml}"
        );
    }
}
