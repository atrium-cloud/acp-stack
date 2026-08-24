//! MCP wizard tests: the streamed add sequence, capability gating, and secret-ref collection.

use super::super::*;
use super::support::*;

use super::super::super::provider::{
    collect_mcp_secret_refs_for_init, collect_missing_provider_refs,
};
use super::super::super::starter_config::{mcp_servers_from_prompted, prompt_mcp_servers};
use crate::secrets::SecretStore;

use serde_json::json;
use std::time::Duration;

/// Answers a streamed prompt sequence one request at a time. The remembered request id keeps the
/// poller from re-reading an answered prompt before the wizard thread wakes and clears it.
struct HostedPromptTranscript {
    session: Arc<HostedInitSession>,
    last_request_id: Option<String>,
}

impl HostedPromptTranscript {
    fn new(session: Arc<HostedInitSession>) -> Self {
        Self {
            session,
            last_request_id: None,
        }
    }

    fn next_pending(&self) -> PublicInputRequest {
        for _ in 0..200 {
            if let Some(input) = lock_unpoisoned(&self.session.inner).pending_input.clone()
                && self.last_request_id.as_ref() != Some(&input.request_id)
            {
                return input;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for the next hosted init input request");
    }

    fn answer(&mut self, kind: HostedPromptKind, response: Value) -> PublicInputRequest {
        let pending = self.next_pending();
        assert_eq!(
            pending.kind,
            kind.as_str(),
            "unexpected prompt on the stream: {pending:?}"
        );
        self.session
            .submit_input(&pending.request_id, response)
            .expect("submit input");
        self.last_request_id = Some(pending.request_id.clone());
        pending
    }
}

fn option_values(request: &PublicInputRequest) -> Vec<&str> {
    request
        .options
        .iter()
        .map(|option| option.value.as_str())
        .collect()
}

/// The step's `offer_http`, derived from a probe fixture so the picker stays tied to the real
/// capability accessor.
fn offer_http_for(advertised: Value) -> bool {
    mcp_capabilities(advertised).supports_mcp_capability("http")
}

/// Runs the post-probe MCP prompts in the order `mcp_configure` drives them.
fn hosted_mcp_wizard(
    session: Arc<HostedInitSession>,
    offer_http: bool,
) -> std::thread::JoinHandle<Result<InitArgs>> {
    let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver { session });
    std::thread::spawn(move || {
        prompt::with_hosted_driver(driver, || {
            let mut args = request_from_json(r#"{"agent":"placebo"}"#)
                .into_init_args()
                .expect("valid request");
            if prompt::confirm(HostedPromptKind::McpAdd, true, "Add MCP servers?", false)? {
                prompt_mcp_servers(true, &mut args, offer_http)?;
            }
            Ok(args)
        })
    })
}

#[test]
fn hosted_mcp_wizard_streams_the_add_sequence_with_kinds_and_stable_values() {
    let session = test_session("init_mcp_wizard");
    let wizard = hosted_mcp_wizard(session.clone(), offer_http_for(json!({"http": true})));
    let mut transcript = HostedPromptTranscript::new(session.clone());

    transcript.answer(HostedPromptKind::McpAdd, json!(true));
    let transport = transcript.answer(HostedPromptKind::McpTransport, json!({"value": "stdio"}));
    assert_eq!(option_values(&transport), ["stdio", "http", "__done"]);
    transcript.answer(HostedPromptKind::McpStdioName, json!("files"));
    transcript.answer(HostedPromptKind::McpStdioCommand, json!("mcp-files"));
    transcript.answer(HostedPromptKind::McpStdioArgs, json!("--root, /data"));
    transcript.answer(HostedPromptKind::McpStdioEnvRefs, json!("FILES_TOKEN"));
    let row_action = transcript.answer(HostedPromptKind::McpRowAction, json!({"value": "done"}));
    assert_eq!(
        option_values(&row_action),
        ["add_another", "discard", "done"]
    );

    transcript.answer(HostedPromptKind::McpTransport, json!({"value": "http"}));
    transcript.answer(HostedPromptKind::McpHttpName, json!("search"));
    transcript.answer(
        HostedPromptKind::McpHttpUrl,
        json!("https://mcp.example.com/mcp"),
    );
    transcript.answer(
        HostedPromptKind::McpHttpHeaders,
        json!("Authorization:SEARCH_API_KEY"),
    );
    transcript.answer(HostedPromptKind::McpRowAction, json!({"value": "done"}));
    transcript.answer(HostedPromptKind::McpTransport, json!({"value": "__done"}));

    let args = wizard.join().expect("wizard thread").expect("mcp wizard");
    assert_eq!(args.prompt_mcp_stdio.len(), 1);
    let stdio = &args.prompt_mcp_stdio[0];
    assert_eq!(stdio.name, "files");
    assert_eq!(stdio.command, "mcp-files");
    assert_eq!(stdio.args, ["--root".to_owned(), "/data".to_owned()]);
    assert_eq!(stdio.env, ["FILES_TOKEN".to_owned()]);
    assert_eq!(args.prompt_mcp_http.len(), 1);
    let http = &args.prompt_mcp_http[0];
    assert_eq!(http.name, "search");
    assert_eq!(http.url, "https://mcp.example.com/mcp");
    assert_eq!(http.headers.len(), 1);
    assert_eq!(http.headers[0].name, "Authorization");
    assert_eq!(http.headers[0].value_ref.as_deref(), Some("SEARCH_API_KEY"));

    let events = serde_json::to_string(&session.events_after(0)).expect("events");
    for kind in [
        "mcp_add",
        "mcp_transport",
        "mcp_stdio_name",
        "mcp_http_headers",
    ] {
        assert!(
            events.contains(&format!(r#""kind":"{kind}""#)),
            "the streamed frames must carry `{kind}`"
        );
    }
}

// A transport the agent never advertised is not offered to a hosted client either.
#[test]
fn hosted_mcp_transport_options_follow_the_probed_capabilities() {
    let session = test_session("init_mcp_transport_options");
    let wizard = hosted_mcp_wizard(session.clone(), offer_http_for(json!({})));
    let mut transcript = HostedPromptTranscript::new(session.clone());

    transcript.answer(HostedPromptKind::McpAdd, json!(true));
    let transport = transcript.answer(HostedPromptKind::McpTransport, json!({"value": "__done"}));
    assert_eq!(option_values(&transport), ["stdio", "__done"]);

    let args = wizard.join().expect("wizard thread").expect("mcp wizard");
    assert!(args.prompt_mcp_stdio.is_empty());
    assert!(args.prompt_mcp_http.is_empty());
}

// Neither the error nor the stream may repeat a pasted credential.
#[test]
fn a_pasted_credential_in_a_header_ref_is_rejected_without_echoing_it() {
    const PASTED: &str = "sk-live-hosted-mcp-header-value";
    // The screening heuristic matches credential prefixes, so `Bearer sk-...` slips past it and
    // reaches the header-name check — which is why that error may quote no input at all.
    // `screened` marks the forms the heuristic itself catches.
    for (index, entry, screened) in [
        (0, format!("Authorization:{PASTED}"), true),
        (1, PASTED.to_owned(), true),
        (2, format!("{PASTED} extra:LINEAR_API_KEY"), true),
        (3, format!("Bearer {PASTED}:LINEAR_API_KEY"), false),
    ] {
        let session = test_session(&format!("init_mcp_header_screen_{index}"));
        let wizard = hosted_mcp_wizard(session.clone(), true);
        let mut transcript = HostedPromptTranscript::new(session.clone());

        transcript.answer(HostedPromptKind::McpAdd, json!(true));
        transcript.answer(HostedPromptKind::McpTransport, json!({"value": "http"}));
        transcript.answer(HostedPromptKind::McpHttpName, json!("search"));
        transcript.answer(
            HostedPromptKind::McpHttpUrl,
            json!("https://mcp.example.com/mcp"),
        );
        transcript.answer(HostedPromptKind::McpHttpHeaders, json!(entry));

        let error = wizard
            .join()
            .expect("wizard thread")
            .expect_err("a pasted credential must be rejected");
        if screened {
            assert!(
                matches!(error, StackError::SecretRefLooksLikeValue { .. }),
                "`{entry}` was not screened: {error:?}"
            );
        } else {
            // Pinned so the case cannot pass by failing somewhere earlier.
            assert!(
                error
                    .public_message()
                    .contains("not a valid HTTP header name"),
                "`{entry}` must be rejected by the header-name check: {error:?}"
            );
        }
        assert!(!error.to_string().contains(PASTED));
        assert!(!error.public_message().contains(PASTED));
        // The rejection travels as a session failure, so assert it through the client's path.
        session.set_error(error.error_code(), error.public_message());
        let events = serde_json::to_string(&session.events_after(0)).expect("events");
        let status = serde_json::to_string(&session.status_snapshot()).expect("status");
        for surface in [events, session.hello_frame(), status] {
            assert!(
                !surface.contains(PASTED),
                "a pasted credential must never be echoed onto {surface}"
            );
        }
    }
}

// The env-ref prompt takes bare ref names, so a dashed token misses every screening prefix and
// lands on the name-shape check — which is why that check may not quote the entry back.
#[test]
fn a_pasted_credential_in_a_stdio_env_ref_is_rejected_without_echoing_it() {
    const PASTED: &str = "xai-9f2c8b1a-4d7e-11ef-9a3b-0242ac120002";
    let session = test_session("init_mcp_env_ref_screen");
    let wizard = hosted_mcp_wizard(session.clone(), true);
    let mut transcript = HostedPromptTranscript::new(session.clone());

    transcript.answer(HostedPromptKind::McpAdd, json!(true));
    transcript.answer(HostedPromptKind::McpTransport, json!({"value": "stdio"}));
    transcript.answer(HostedPromptKind::McpStdioName, json!("files"));
    transcript.answer(HostedPromptKind::McpStdioCommand, json!("mcp-files"));
    transcript.answer(HostedPromptKind::McpStdioArgs, json!(""));
    transcript.answer(HostedPromptKind::McpStdioEnvRefs, json!(PASTED));

    let error = wizard
        .join()
        .expect("wizard thread")
        .expect_err("a pasted credential must be rejected");
    // Pinned so the case cannot pass by failing earlier.
    assert!(
        error.public_message().contains("secret ref name must use"),
        "`{PASTED}` must be rejected by the ref-name check: {error:?}"
    );
    assert!(!error.to_string().contains(PASTED));
    assert!(!error.public_message().contains(PASTED));
    session.set_error(error.error_code(), error.public_message());
    let events = serde_json::to_string(&session.events_after(0)).expect("events");
    let status = serde_json::to_string(&session.status_snapshot()).expect("status");
    for surface in [events, session.hello_frame(), status] {
        assert!(
            !surface.contains(PASTED),
            "a pasted credential must never be echoed onto {surface}"
        );
    }
}

// The collected secret must reach the store without appearing in the event history.
#[test]
fn hosted_mcp_secret_values_are_collected_as_password_prompts() {
    const SECRET: &str = "files-token-value";
    let home = tempfile::tempdir().expect("tempdir");
    let session = test_session("init_mcp_secret_refs");
    let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver {
        session: session.clone(),
    });
    let home_path = home.path().to_path_buf();
    let collector = std::thread::spawn(move || {
        let mut store = SecretStore::open_or_create(&home_path).expect("secret store");
        let stored = prompt::with_hosted_driver(driver, || {
            collect_mcp_secret_refs_for_init(true, &config_with_mcp_env_ref(), &mut store)
        })?;
        Ok::<_, StackError>((stored, store))
    });

    let mut transcript = HostedPromptTranscript::new(session.clone());
    let pending = transcript.answer(HostedPromptKind::SecretRefValue, json!(SECRET));
    assert_eq!(pending.style, "password");
    assert_eq!(pending.prompt, "FILES_TOKEN");

    let (stored, store) = collector
        .join()
        .expect("collector thread")
        .expect("collect");
    assert_eq!(stored, ["FILES_TOKEN".to_owned()]);
    assert_eq!(store.get("FILES_TOKEN").expect("stored ref"), SECRET);
    let events = serde_json::to_string(&session.events_after(0)).expect("events");
    assert!(
        !events.contains(SECRET),
        "a collected secret value must never reach the stream"
    );
}

#[test]
fn hosted_deferral_soft_passes_custom_provider_key_without_prompting() {
    let home = tempfile::tempdir().expect("tempdir");
    let session = test_session_deferring_credentials("init_provider_key_soft_pass");
    let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver {
        session: session.clone(),
    });
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    let mut config = config::load_config_from_str(include_str!(
        "../../../../../tests/fixtures/valid-opencode-stack.toml"
    ))
    .expect("fixture config");
    config.agent.provider = Some(config::AgentProviderConfig {
        id: "my-custom".to_owned(),
        model: Some("my-model".to_owned()),
        api_key_ref: Some("CUSTOM_KEY".to_owned()),
        custom: Some(config::AgentCustomProviderConfig {
            name: "My Custom".to_owned(),
            base_url: "https://example.test/v1".to_owned(),
            api: config::CustomProviderApi::default(),
            model_name: None,
            context: config::DEFAULT_CUSTOM_MODEL_CONTEXT,
            output_max_tokens: config::DEFAULT_CUSTOM_MODEL_OUTPUT_MAX_TOKENS,
        }),
    });
    // No client interaction: a deferred ref never streams a value prompt, so the collection
    // completes on this thread alone.
    prompt::with_hosted_driver(driver, || {
        collect_missing_provider_refs(
            true,
            &mut store,
            &config,
            Some("my-custom"),
            &["CUSTOM_KEY".to_owned()],
        )
    })
    .expect("declared deferral soft-passes the provider ref");
    assert_deferral_stream_without_value_prompt(&session);
}

#[test]
fn hosted_deferral_soft_passes_mapped_provider_key_without_prompting() {
    let home = tempfile::tempdir().expect("tempdir");
    let session = test_session_deferring_credentials("init_mapped_provider_key_soft_pass");
    let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver {
        session: session.clone(),
    });
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    let mut config = config::load_config_from_str(include_str!(
        "../../../../../tests/fixtures/valid-opencode-stack.toml"
    ))
    .expect("fixture config");
    config.agent.env = vec!["OPENROUTER_API_KEY".to_owned()];
    config.agent.provider = Some(config::AgentProviderConfig {
        id: "openrouter".to_owned(),
        model: Some("some/model".to_owned()),
        api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
        custom: None,
    });
    prompt::with_hosted_driver(driver, || {
        collect_missing_provider_refs(
            true,
            &mut store,
            &config,
            Some("openrouter"),
            &["OPENROUTER_API_KEY".to_owned()],
        )
    })
    .expect("declared deferral soft-passes the mapped provider ref");
    assert_deferral_stream_without_value_prompt(&session);
}

fn assert_deferral_stream_without_value_prompt(session: &Arc<HostedInitSession>) {
    let events = serde_json::to_string(&session.events_after(0)).expect("events");
    assert!(
        events.contains("not present yet"),
        "deferral progress must reach the stream: {events}"
    );
    assert!(
        !events.contains("provider_api_key_value"),
        "the value prompt must not be streamed under a declared deferral: {events}"
    );
}

fn config_with_mcp_env_ref() -> config::Config {
    let mut config = config::load_config_from_str(include_str!(
        "../../../../../tests/fixtures/valid-opencode-stack.toml"
    ))
    .expect("fixture config");
    config.mcp.servers = mcp_servers_from_prompted(
        &[InitMcpStdioServer {
            name: "files".to_owned(),
            command: "mcp-files".to_owned(),
            args: Vec::new(),
            env: vec!["FILES_TOKEN".to_owned()],
        }],
        &[],
    )
    .expect("declared servers");
    config
}
