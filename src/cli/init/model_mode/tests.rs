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

/// Effort rides codex-acp's real shape: id `reasoning_effort` under the
/// reserved `thought_level` category, so the lane's category match (not the id
/// fallback) is what these tests exercise.
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

// Nothing to pick is not an error: the operator is told through the state
// report, not by failing an init that had no explicit request.
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

// Model twin of the mode picker skip: an agent that advertises no `model`
// option (e.g. amp-acp older than v0.8.0) must skip the lane rather than
// fail init when no `--model` was requested.
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

/// Answers every picker with its first option and keeps what was offered,
/// so a test can assert on the option ids that reached the wire.
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

// Option ids are answerable over the wire and the shared prompt entry point
// asserts they are unique, so a harness advertising the same mode twice
// would take a debug build down with it.
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
fn discovery_only_retracts_a_lane_the_registry_claimed() {
    let driver = std::sync::Arc::new(prompt::RecordingPromptDriver::default());
    prompt::with_hosted_driver(driver.clone(), || {
        // Registry says both lanes exist; the harness advertises neither.
        emit_discovery_applicability_corrections(&response_with(&[], &[]), true, true, false);
        // Registry says neither; the harness advertises modes anyway. Init
        // will not write a mode for such an agent, so nothing is claimed.
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
