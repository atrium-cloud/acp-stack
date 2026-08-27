use agent_client_protocol::schema::v1::{NewSessionResponse, SessionConfigOption};
use clap::Parser;

use super::*;

#[derive(clap::Parser)]
struct TestInitArgs {
    #[command(flatten)]
    args: InitArgs,
}

fn parse_init_args(argv: &[&str]) -> InitArgs {
    let mut all = vec!["init-test"];
    all.extend_from_slice(argv);
    TestInitArgs::parse_from(all).args
}

/// Mirrors the shape `fetch_session_config` returns for a fixture run.
fn response_with(models: &[&str], modes: &[&str]) -> NewSessionResponse {
    let mut options = Vec::new();
    for (id, values) in [("model", models), ("mode", modes)] {
        if values.is_empty() {
            continue;
        }
        let option: SessionConfigOption = serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "category": id,
            "type": "select",
            "currentValue": values[0],
            "options": values
                .iter()
                .map(|value| serde_json::json!({ "value": value, "name": value }))
                .collect::<Vec<_>>(),
        }))
        .expect("session config option fixture parses");
        options.push(option);
    }
    NewSessionResponse::new("test").config_options(options)
}

/// Effort rides codex-acp's real shape: id `reasoning_effort` under the reserved
/// `thought_level` category, so these tests exercise the category match, not the id fallback.
fn response_with_efforts(efforts: &[&str]) -> NewSessionResponse {
    let option: SessionConfigOption = serde_json::from_value(serde_json::json!({
        "id": "reasoning_effort",
        "name": "Reasoning Effort",
        "category": "thought_level",
        "type": "select",
        "currentValue": efforts[0],
        "options": efforts
            .iter()
            .map(|value| serde_json::json!({ "value": value, "name": value }))
            .collect::<Vec<_>>(),
    }))
    .expect("session config option fixture parses");
    NewSessionResponse::new("test").config_options(vec![option])
}

fn amp_config() -> Config {
    let mut config = crate::config::load_config_from_str(include_str!(
        "../../../../tests/fixtures/valid-opencode-stack.toml"
    ))
    .expect("fixture config");
    config.agent.id = "amp".to_owned();
    config
}

#[test]
fn explicit_mode_is_written_and_settled_at_the_write() {
    let mut config = amp_config();
    let args = parse_init_args(&["--mode", "bypass"]);
    let driver = std::sync::Arc::new(prompt::RecordingPromptDriver::default());

    let action = prompt::with_hosted_driver(driver.clone(), || {
        configure_mode_for_init(
            &args,
            &mut config,
            Path::new("acps-config.toml"),
            &response_with(&[], &["default", "bypass"]),
            true,
        )
    })
    .expect("advertised mode is accepted");

    assert_eq!(action, ModelModeAction::Set);
    assert_eq!(config.agent.mode.as_deref(), Some("bypass"));
    assert_eq!(
        driver.recorded(),
        vec![InitStateSignal::CategorySettled {
            category: InitCategory::Mode,
            value: Some("bypass".to_owned()),
        }],
    );
}

#[test]
fn explicit_mode_that_is_not_advertised_lists_the_advertised_modes() {
    let mut config = amp_config();
    let args = parse_init_args(&["--mode", "bogus"]);

    let error = configure_mode_for_init(
        &args,
        &mut config,
        Path::new("acps-config.toml"),
        &response_with(&[], &["default", "bypass"]),
        true,
    )
    .expect_err("unadvertised mode must be rejected");

    assert!(
        error
            .to_string()
            .contains("advertised modes: [bypass, default]"),
        "error: {error}"
    );
    assert!(config.agent.mode.is_none());
}

// Nothing to pick is not an error: the state report tells the operator instead.
#[test]
fn a_mode_picker_with_nothing_advertised_skips_without_writing() {
    let mut config = amp_config();
    let args = parse_init_args(&[]);

    let action = configure_mode_for_init(
        &args,
        &mut config,
        Path::new("acps-config.toml"),
        &response_with(&["openai/gpt-5.5"], &[]),
        true,
    )
    .expect("an empty mode list is not a failure");

    assert_eq!(action, ModelModeAction::Skipped);
    assert!(config.agent.mode.is_none());
}

// Model twin of the mode picker skip; amp-acp before v0.8.0 advertises no `model` option.
#[test]
fn a_model_picker_with_nothing_advertised_skips_without_writing() {
    let mut config = amp_config();
    let args = parse_init_args(&[]);
    let tempdir = tempfile::tempdir().expect("tempdir");

    let action = configure_model_for_init(
        &args,
        tempdir.path(),
        &mut config,
        Path::new("acps-config.toml"),
        &response_with(&[], &["default", "bypass"]),
        "Amp Code",
        false,
    )
    .expect("an absent model option must not fail init");

    assert_eq!(action, ModelModeAction::Skipped);
    assert!(config.agent.model.is_none());
}

#[test]
fn explicit_effort_is_written_and_settled_at_the_write() {
    let mut config = amp_config();
    let args = parse_init_args(&["--effort", "high"]);
    let driver = std::sync::Arc::new(prompt::RecordingPromptDriver::default());

    let action = prompt::with_hosted_driver(driver.clone(), || {
        configure_effort_for_init(
            &args,
            &mut config,
            Path::new("acps-config.toml"),
            &response_with_efforts(&["low", "medium", "high"]),
            true,
        )
    })
    .expect("advertised effort is accepted");

    assert_eq!(action, ModelModeAction::Set);
    assert_eq!(config.agent.effort.as_deref(), Some("high"));
    assert_eq!(
        driver.recorded(),
        vec![InitStateSignal::CategorySettled {
            category: InitCategory::Effort,
            value: Some("high".to_owned()),
        }],
    );
}

#[test]
fn explicit_effort_that_is_not_advertised_lists_the_advertised_efforts() {
    let mut config = amp_config();
    let args = parse_init_args(&["--effort", "bogus"]);

    let error = configure_effort_for_init(
        &args,
        &mut config,
        Path::new("acps-config.toml"),
        &response_with_efforts(&["low", "medium", "high"]),
        true,
    )
    .expect_err("unadvertised effort must be rejected");

    assert!(
        error
            .to_string()
            .contains("advertised efforts: [high, low, medium]"),
        "error: {error}"
    );
    assert!(config.agent.effort.is_none());
}

#[test]
fn an_effort_picker_with_nothing_advertised_skips_without_writing() {
    let mut config = amp_config();
    let args = parse_init_args(&[]);

    let action = configure_effort_for_init(
        &args,
        &mut config,
        Path::new("acps-config.toml"),
        &response_with(&["openai/gpt-5.5"], &["build"]),
        true,
    )
    .expect("an empty effort list is not a failure");

    assert_eq!(action, ModelModeAction::Skipped);
    assert!(config.agent.effort.is_none());
}

/// Answers every picker with its first option and records what was offered.
#[derive(Default)]
struct FirstChoiceDriver {
    offered: std::sync::Mutex<Vec<Vec<String>>>,
}

impl prompt::HostedPromptDriver for FirstChoiceDriver {
    fn select(
        &self,
        request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
        self.offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(
                request
                    .items
                    .iter()
                    .map(|item| item.value.clone())
                    .collect(),
            );
        Ok(prompt::HostedPromptOutcome::Handled(Some(0)))
    }

    fn confirm(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<bool>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn text(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn password(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn progress(&self, _message: String) {}

    fn result(&self, _payload: serde_json::Value) {}
}

// The shared prompt entry point asserts option ids are unique, so a harness advertising the
// same mode twice would take a debug build down with it.
#[test]
fn a_repeated_advertised_value_is_offered_once() {
    let mut config = amp_config();
    let args = parse_init_args(&[]);
    let driver = std::sync::Arc::new(FirstChoiceDriver::default());

    let action = prompt::with_hosted_driver(driver.clone(), || {
        configure_mode_for_init(
            &args,
            &mut config,
            Path::new("acps-config.toml"),
            &response_with(&[], &["default", "bypass", "default"]),
            true,
        )
    })
    .expect("a duplicated advertised value is not a failure");

    assert_eq!(action, ModelModeAction::Set);
    assert_eq!(config.agent.mode.as_deref(), Some("bypass"));
    assert_eq!(
        driver
            .offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_slice(),
        [vec![
            "bypass".to_owned(),
            "default".to_owned(),
            "__skip".to_owned()
        ]]
    );
}

#[test]
fn deferred_mapped_credential_writes_explicit_model_without_discovery() {
    let home = tempfile::tempdir().expect("tempdir");
    let secrets = crate::secrets::new_shared_secret_store(
        crate::secrets::SecretStore::open_or_create(home.path()).expect("secret store"),
    );
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let mut config = crate::config::load_config_from_str(include_str!(
        "../../../../tests/fixtures/valid-opencode-stack.toml"
    ))
    .expect("fixture config");
    config.agent.env = vec!["OPENROUTER_API_KEY".to_owned()];
    config.agent.provider = Some(crate::config::AgentProviderConfig {
        id: "openrouter".to_owned(),
        model: None,
        api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
        custom: None,
    });
    let args = parse_init_args(&["--model", "some/model"]);

    // Without the deferral declaration the pending credential still fails the flag loudly.
    let plain = std::sync::Arc::new(prompt::RecordingPromptDriver::default());
    prompt::with_hosted_driver(plain, || {
        configure_model_and_mode_for_init(
            &args,
            home.path(),
            &registry,
            &mut config,
            Path::new("acps-config.toml"),
            &secrets,
        )
    })
    .expect_err("undeclared run cannot validate --model against a pending credential");

    let deferring =
        std::sync::Arc::new(prompt::RecordingPromptDriver::deferring_provider_credentials());
    let outcome = prompt::with_hosted_driver(deferring, || {
        configure_model_and_mode_for_init(
            &args,
            home.path(),
            &registry,
            &mut config,
            Path::new("acps-config.toml"),
            &secrets,
        )
    })
    .expect("declared deferral accepts the explicit model without discovery");
    assert_eq!(outcome.model_action, ModelModeAction::Set);
    assert_eq!(
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref()),
        Some("some/model"),
        "the explicit model lands in the provider block unvalidated"
    );
}

#[test]
fn discovery_only_retracts_a_lane_the_registry_claimed() {
    let driver = std::sync::Arc::new(prompt::RecordingPromptDriver::default());
    prompt::with_hosted_driver(driver.clone(), || {
        // Registry claims both lanes; the harness advertises neither.
        emit_discovery_applicability_corrections(&response_with(&[], &[]), true, true, false);
        // Registry claims neither, so the harness advertising modes claims nothing.
        emit_discovery_applicability_corrections(
            &response_with(&[], &["plan"]),
            false,
            false,
            false,
        );
    });

    let recorded = driver.recorded();
    assert_eq!(
        recorded,
        [InitCategory::Model, InitCategory::Mode].map(|category| {
            InitStateSignal::CategoryApplicability {
                category,
                applicable: false,
                source: ApplicabilitySource::Discovery,
                reason: Some(format!(
                    "agent advertised no `{}` values on session/new",
                    match category {
                        InitCategory::Model => "model",
                        _ => "mode",
                    }
                )),
            }
        }),
        "only the registry-claimed lanes the harness contradicts are corrected"
    );
}
