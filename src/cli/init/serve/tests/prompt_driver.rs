//! Prompt-driver tests: what the hosted driver streams, and how answers resolve.

use super::super::*;
use super::support::*;

use serde_json::json;

#[test]
fn hosted_driver_accepts_provider_password_and_model_responses() {
    let provider = send_select_response(
        HostedPromptKind::ProviderId,
        "provider for opencode",
        &["OpenRouter (openrouter)", "DeepSeek (deepseek)"],
        json!("OpenRouter (openrouter)"),
    );
    assert_eq!(provider, HostedPromptOutcome::Handled(Some(0)));

    let model = send_select_response(
        HostedPromptKind::Model,
        "select model",
        &["deepseek-v4-flash", "openai/gpt-5-mini"],
        json!({ "index": 1 }),
    );
    assert_eq!(model, HostedPromptOutcome::Handled(Some(1)));

    let session = test_session("init_driver_password");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = HostedPromptRequest {
        kind: HostedPromptKind::ProviderApiKeyValue,
        style: HostedPromptStyle::Password,
        prompt: "OPENROUTER_API_KEY".to_owned(),
        required: true,
        default: None,
        items: Vec::new(),
        inspection: None,
    };
    let handle = std::thread::spawn(move || driver.password(request));
    let pending = wait_for_pending_input(&session);
    session
        .submit_input(&pending.request_id, json!("sk-hosted-secret"))
        .expect("submit password");
    let password = handle.join().expect("driver thread").expect("password");
    assert_eq!(
        password,
        HostedPromptOutcome::Handled(Some("sk-hosted-secret".to_owned()))
    );
    let events = serde_json::to_string(&session.events_after(0)).expect("events");
    assert!(!events.contains("sk-hosted-secret"));
}

#[test]
fn hosted_driver_streams_testflight_confirmation() {
    let session = test_session("init_driver_testflight");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = HostedPromptRequest {
        kind: HostedPromptKind::TestflightConfirm,
        style: HostedPromptStyle::Confirm,
        prompt: "run testflight now?".to_owned(),
        required: true,
        default: Some(false),
        items: Vec::new(),
        inspection: None,
    };
    let handle = std::thread::spawn(move || driver.confirm(request));
    let pending = wait_for_pending_input(&session);
    assert_eq!(pending.prompt, "run testflight now?");
    session
        .submit_input(&pending.request_id, json!(true))
        .expect("submit confirm");
    let confirm = handle.join().expect("driver thread").expect("confirm");
    assert_eq!(confirm, HostedPromptOutcome::Handled(true));
}

/// Answers a streamed testflight confirm with a raw client frame, so the answer
/// goes through the same parser the websocket uses. `fields` is the frame body
/// after `request_id`.
fn testflight_confirm_answer(session_id: &str, fields: &str) -> ConfirmAnswer {
    let session = test_session(session_id);
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = HostedPromptRequest {
        kind: HostedPromptKind::TestflightConfirm,
        style: HostedPromptStyle::Confirm,
        prompt: "run testflight now?".to_owned(),
        required: true,
        default: Some(false),
        items: Vec::new(),
        inspection: None,
    };
    let handle = std::thread::spawn(move || driver.confirm_with_deferral(request));
    let pending = wait_for_pending_input(&session);
    let frame = format!(
        r#"{{"type":"input","request_id":"{}",{fields}}}"#,
        pending.request_id
    );
    match handle_client_frame(&session, &frame) {
        ClientFrameOutcome::None => {}
        ClientFrameOutcome::Send(response) | ClientFrameOutcome::Close(response) => {
            panic!("input frame was rejected: {response}")
        }
    }
    match handle.join().expect("driver thread").expect("confirm") {
        HostedPromptOutcome::Handled(answer) => answer,
        HostedPromptOutcome::Unhandled => panic!("a streamed confirm must be handled"),
    }
}

#[test]
fn hosted_testflight_confirm_decodes_the_deferred_sibling() {
    assert_eq!(
        testflight_confirm_answer("init_confirm_accept", r#""value":true"#),
        ConfirmAnswer {
            value: true,
            deferred: false
        }
    );
    // No flag is a decline, which is what every client that predates the field
    // sends and what the operator-facing terminal path means.
    assert_eq!(
        testflight_confirm_answer("init_confirm_decline", r#""value":false"#),
        ConfirmAnswer {
            value: false,
            deferred: false
        }
    );
    assert_eq!(
        testflight_confirm_answer("init_confirm_deferred", r#""value":false,"deferred":true"#),
        ConfirmAnswer {
            value: false,
            deferred: true
        }
    );
    // Explicit `false` is a decline too, so a client can send the field always.
    assert_eq!(
        testflight_confirm_answer(
            "init_confirm_not_deferred",
            r#""value":false,"deferred":false"#
        ),
        ConfirmAnswer {
            value: false,
            deferred: false
        }
    );
}

/// The `deferred` rollout depends on this: a client frame carrying a field this
/// binary does not know is accepted, not rejected.
#[test]
fn hosted_client_frames_tolerate_unknown_fields() {
    assert_eq!(
        testflight_confirm_answer(
            "init_confirm_unknown_field",
            r#""value":false,"deferred":true,"invented_by_a_newer_backend":42"#
        ),
        ConfirmAnswer {
            value: false,
            deferred: true
        }
    );
}

#[test]
fn hosted_driver_streams_redacted_native_config_review() {
    let inspected = crate::runtime::agent::native_config_import::inspect_native_config(
        "opencode",
        Some("opencode.json"),
        r#"{"theme":"raw-secret-value","model":"openai/gpt-5"}"#,
    )
    .expect("inspect");
    let inspection = inspected.inspection().clone();
    let revision = inspection.revision.clone();
    let session = test_session("init_driver_native_config");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = HostedPromptRequest {
        kind: HostedPromptKind::NativeConfigReview,
        style: HostedPromptStyle::NativeConfigReview,
        prompt: "Review native Agent config".to_owned(),
        required: true,
        default: None,
        items: Vec::new(),
        inspection: Some(inspection),
    };
    let handle = std::thread::spawn(move || driver.native_config_review(request));
    let pending = wait_for_pending_input(&session);
    assert_eq!(pending.kind, "native_config_review");
    assert_eq!(pending.style, "native_config_review");
    assert_eq!(
        pending.inspection.as_ref().expect("inspection").revision,
        revision
    );
    let serialized = serde_json::to_string(&pending).expect("serialize");
    assert!(!serialized.contains("raw-secret-value"));
    session
        .submit_input(
            &pending.request_id,
            json!({
                "revision": revision,
                "selected_managed_field_ids": ["provider", "model"],
                "executable_settings_acknowledged": false
            }),
        )
        .expect("submit review");
    let selection = handle.join().expect("driver thread").expect("review");
    assert!(matches!(selection, HostedPromptOutcome::Handled(_)));
    let events = serde_json::to_string(&session.events_after(0)).expect("events");
    assert!(!events.contains("raw-secret-value"));
}

#[test]
fn hosted_driver_leaves_non_bootstrap_text_prompts_unhandled() {
    let session = test_session("init_driver_text");
    let driver = SessionPromptDriver { session };
    let request = HostedPromptRequest {
        kind: HostedPromptKind::ConfigSourcePath,
        style: HostedPromptStyle::Text,
        prompt: "acps-config.toml path".to_owned(),
        required: true,
        default: None,
        items: Vec::new(),
        inspection: None,
    };
    let outcome = driver.text(request).expect("text");
    assert_eq!(outcome, HostedPromptOutcome::Unhandled);
}

/// The five prompt strings the update-policy flow emits. The Text entries
/// are the custom-frequency input behind the select's Custom branch
/// (prompts.rs renders them from the consumer's DurationLimits: stack =
/// day/week min 1 day, agent = hour/day/week min 1 hour).
const UPDATE_POLICY_PROMPTS: [(HostedPromptKind, HostedPromptStyle, &str); 5] = [
    (
        HostedPromptKind::StackUpdatePolicy,
        HostedPromptStyle::Select,
        "acp-stack auto-update",
    ),
    (
        HostedPromptKind::UpdateFrequency,
        HostedPromptStyle::Select,
        "update frequency",
    ),
    (
        HostedPromptKind::AgentUpdateEnabled,
        HostedPromptStyle::Confirm,
        "Auto-update this agent's harness?",
    ),
    (
        HostedPromptKind::UpdateFrequencyCustom,
        HostedPromptStyle::Text,
        "frequency (e.g. 1d, 3w; minimum 1 day)",
    ),
    (
        HostedPromptKind::UpdateFrequencyCustom,
        HostedPromptStyle::Text,
        "frequency (e.g. 1h, 3w; minimum 1 hour)",
    ),
];

#[test]
fn hosted_driver_never_streams_update_policy_prompts() {
    // The api.md/init.md contract promises the stack- and agent-update
    // prompts stay out of the streamed set — hosted clients supply these
    // up front via `stack_update`/`agent_update`.
    for (kind, style, text) in UPDATE_POLICY_PROMPTS {
        let request = hosted_test_request(kind, style, text, &[]);
        assert!(
            !should_handle_hosted_prompt(&request),
            "update-policy prompt `{text}` must not be streamed to hosted clients"
        );
    }
}

#[test]
fn hosted_prompt_allow_list_keys_off_kind_not_prompt_text() {
    // The same wording under a hostable kind streams, which is what proves
    // the decision moved off string matching: rewording a prompt can no
    // longer change hostability, and only the kind can.
    for (_, style, text) in UPDATE_POLICY_PROMPTS {
        let request = hosted_test_request(HostedPromptKind::Model, style, text, &[]);
        assert!(
            should_handle_hosted_prompt(&request),
            "prompt `{text}` must stream once carried by a hostable kind"
        );
    }
}

#[test]
fn hostable_kinds_carry_their_wire_kind_into_input_required_and_pending_input() {
    for (kind, style) in [
        (HostedPromptKind::Agent, HostedPromptStyle::SearchableSelect),
        (
            HostedPromptKind::ProviderId,
            HostedPromptStyle::SearchableSelect,
        ),
        (HostedPromptKind::Model, HostedPromptStyle::SearchableSelect),
        (HostedPromptKind::Mode, HostedPromptStyle::SearchableSelect),
        (HostedPromptKind::McpTransport, HostedPromptStyle::Select),
        (HostedPromptKind::McpRowAction, HostedPromptStyle::Select),
        (HostedPromptKind::McpAdd, HostedPromptStyle::Confirm),
        (
            HostedPromptKind::TestflightConfirm,
            HostedPromptStyle::Confirm,
        ),
        (HostedPromptKind::ProviderName, HostedPromptStyle::Text),
        (HostedPromptKind::BaseUrl, HostedPromptStyle::Text),
        (HostedPromptKind::ApiKeyRef, HostedPromptStyle::Text),
        (HostedPromptKind::McpStdioName, HostedPromptStyle::Text),
        (HostedPromptKind::McpStdioCommand, HostedPromptStyle::Text),
        (HostedPromptKind::McpStdioArgs, HostedPromptStyle::Text),
        (HostedPromptKind::McpStdioEnvRefs, HostedPromptStyle::Text),
        (HostedPromptKind::McpHttpName, HostedPromptStyle::Text),
        (HostedPromptKind::McpHttpUrl, HostedPromptStyle::Text),
        (HostedPromptKind::McpHttpHeaders, HostedPromptStyle::Text),
        (
            HostedPromptKind::ProviderApiKeyValue,
            HostedPromptStyle::Password,
        ),
        (
            HostedPromptKind::SecretRefValue,
            HostedPromptStyle::Password,
        ),
    ] {
        let request = hosted_test_request(kind, style, "prompt", &["alpha", "beta"]);
        assert!(
            should_handle_hosted_prompt(&request),
            "kind `{}` must be streamed to hosted clients",
            kind.as_str()
        );

        let session = test_session("init_kind_wire");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let handle = std::thread::spawn(move || driver.select(request));
        let pending = wait_for_pending_input(&session);
        assert_eq!(pending.kind, kind.as_str());
        let frame = recorded_frame(&session, 2);
        assert!(
            frame.contains(&format!(r#""kind":"{}""#, kind.as_str())),
            "input_required frame must carry the kind: {frame}"
        );
        session
            .submit_input(&pending.request_id, Value::Null)
            .expect("submit input");
        handle.join().expect("driver thread").expect("select");
    }
}

#[test]
fn select_options_carry_stable_values_distinct_from_labels() {
    let session = test_session("init_option_values");
    let driver = SessionPromptDriver {
        session: session.clone(),
    };
    let request = hosted_test_request(
        HostedPromptKind::Model,
        HostedPromptStyle::SearchableSelect,
        "select model",
        &["alpha", "beta"],
    );
    let handle = std::thread::spawn(move || driver.select(request));
    let pending = wait_for_pending_input(&session);
    let values: Vec<_> = pending
        .options
        .iter()
        .map(|option| option.value.as_str())
        .collect();
    assert_eq!(values, ["id_alpha", "id_beta"]);
    for option in &pending.options {
        assert_ne!(option.value, option.label);
    }
    session
        .submit_input(&pending.request_id, Value::Null)
        .expect("submit input");
    handle.join().expect("driver thread").expect("select");
}

#[test]
fn select_answers_resolve_by_value_index_label_or_null() {
    let labels = ["alpha", "beta"];
    let by_value = send_select_response(
        HostedPromptKind::Model,
        "select model",
        &labels,
        json!({"value": "id_beta"}),
    );
    assert_eq!(by_value, HostedPromptOutcome::Handled(Some(1)));

    let by_index = send_select_response(
        HostedPromptKind::Model,
        "select model",
        &labels,
        json!({"index": 0}),
    );
    assert_eq!(by_index, HostedPromptOutcome::Handled(Some(0)));

    let by_label = send_select_response(
        HostedPromptKind::Model,
        "select model",
        &labels,
        json!("beta"),
    );
    assert_eq!(by_label, HostedPromptOutcome::Handled(Some(1)));

    let skipped = send_select_response(
        HostedPromptKind::Model,
        "select model",
        &labels,
        Value::Null,
    );
    assert_eq!(skipped, HostedPromptOutcome::Handled(None));
}

#[test]
fn unknown_select_value_is_rejected_as_invalid_param() {
    let error = select_result(
        HostedPromptKind::Model,
        "select model",
        &["alpha", "beta"],
        json!({"value": "id_gamma"}),
    )
    .expect_err("unknown option value must be rejected");
    assert!(matches!(
        error,
        StackError::InvalidParam { field: "init", .. }
    ));
    // Bare strings match labels only, so an id sent that way is unknown too.
    let error = select_result(
        HostedPromptKind::Model,
        "select model",
        &["alpha", "beta"],
        json!("id_alpha"),
    )
    .expect_err("an option id is not a label");
    assert!(matches!(
        error,
        StackError::InvalidParam { field: "init", .. }
    ));
}
