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

fn response_with_typed_and_generic_options() -> NewSessionResponse {
    let mut response = response_with(&["openai/gpt-5.5"], &["build", "plan"]);
    let options = response.config_options.get_or_insert_default();
    options.push(
        serde_json::from_value(serde_json::json!({
            "id": "reasoning_effort",
            "name": "Reasoning Effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": "medium",
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "medium", "name": "Medium" }
            ]
        }))
        .expect("effort option"),
    );
    options.push(
        serde_json::from_value(serde_json::json!({
            "id": "agent.persona",
            "name": "Persona",
            "description": "Response style",
            "category": "_behavior",
            "type": "select",
            "currentValue": "balanced",
            "options": [
                { "value": "balanced", "name": "Balanced" },
                { "value": "research", "name": "Research" }
            ]
        }))
        .expect("generic select option"),
    );
    options.push(
        serde_json::from_value(serde_json::json!({
            "id": "fast",
            "name": "Fast mode",
            "type": "boolean",
            "currentValue": false
        }))
        .expect("generic boolean option"),
    );
    response
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
            None,
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
        None,
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
        None,
    )
    .expect("an empty mode list is not a failure");

    assert_eq!(action, ModelModeAction::Skipped);
    assert!(config.agent.mode.is_none());
}

#[test]
fn registry_default_mode_lands_unattended_when_advertised() {
    let mut config = amp_config();
    let args = parse_init_args(&[]);

    let action = configure_mode_for_init(
        &args,
        &mut config,
        Path::new("acps-config.toml"),
        &response_with(&[], &["default", "yolo"]),
        false,
        Some("yolo"),
    )
    .expect("advertised default mode is accepted");

    assert_eq!(action, ModelModeAction::Set);
    assert_eq!(config.agent.mode.as_deref(), Some("yolo"));
}

#[test]
fn registry_default_mode_is_skipped_when_not_advertised() {
    let mut config = amp_config();
    let args = parse_init_args(&[]);

    let action = configure_mode_for_init(
        &args,
        &mut config,
        Path::new("acps-config.toml"),
        &response_with(&[], &["default"]),
        false,
        Some("yolo"),
    )
    .expect("a default that cannot land is not a failure");

    assert_eq!(action, ModelModeAction::Skipped);
    assert!(config.agent.mode.is_none());
}

#[test]
fn explicit_mode_wins_over_the_registry_default() {
    let mut config = amp_config();
    let args = parse_init_args(&["--mode", "default"]);

    configure_mode_for_init(
        &args,
        &mut config,
        Path::new("acps-config.toml"),
        &response_with(&[], &["default", "yolo"]),
        false,
        Some("yolo"),
    )
    .expect("explicit mode is accepted");

    assert_eq!(config.agent.mode.as_deref(), Some("default"));
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
            Path::new("home"),
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
        Path::new("home"),
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
        Path::new("home"),
        &mut config,
        Path::new("acps-config.toml"),
        &response_with(&["openai/gpt-5.5"], &["build"]),
        true,
    )
    .expect("an empty effort list is not a failure");

    assert_eq!(action, ModelModeAction::Skipped);
    assert!(config.agent.effort.is_none());
}

/// Answers every picker with its first option and records what each was offered.
#[derive(Default)]
struct FirstChoiceDriver {
    offered: std::sync::Mutex<Vec<(prompt::HostedPromptKind, Vec<String>)>>,
}

impl FirstChoiceDriver {
    fn offered_for(&self, kind: prompt::HostedPromptKind) -> Vec<Vec<String>> {
        self.offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(recorded, _)| *recorded == kind)
            .map(|(_, values)| values.clone())
            .collect()
    }
}

#[derive(Default)]
struct GenericConfigDriver {
    offered: std::sync::Mutex<Vec<String>>,
    skip_all: bool,
}

impl prompt::HostedPromptDriver for GenericConfigDriver {
    fn select(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
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

    fn config_option(
        &self,
        request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<crate::config::AgentConfigOptionValue>>> {
        let option = request.config_option.expect("advertised option metadata");
        self.offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(option.id.clone());
        if self.skip_all {
            return Ok(prompt::HostedPromptOutcome::Handled(None));
        }
        let value = match option.id.as_str() {
            "agent.persona" => crate::config::AgentConfigOptionValue::Text("research".to_owned()),
            "fast" => crate::config::AgentConfigOptionValue::Bool(true),
            other => panic!("typed option was prompted generically: {other}"),
        };
        Ok(prompt::HostedPromptOutcome::Handled(Some(value)))
    }

    fn progress(&self, _message: String) {}

    fn result(&self, _payload: serde_json::Value) {}
}

impl prompt::HostedPromptDriver for FirstChoiceDriver {
    fn select(
        &self,
        request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
        self.offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((
                request.kind,
                request
                    .items
                    .iter()
                    .map(|item| item.value.clone())
                    .collect(),
            ));
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
            None,
        )
    })
    .expect("a duplicated advertised value is not a failure");

    assert_eq!(action, ModelModeAction::Set);
    assert_eq!(config.agent.mode.as_deref(), Some("bypass"));
    assert_eq!(
        driver.offered_for(prompt::HostedPromptKind::Mode),
        [vec![
            "bypass".to_owned(),
            "default".to_owned(),
            "__skip".to_owned()
        ]]
    );
}

#[test]
fn generic_options_are_prompted_once_without_duplicating_typed_lanes() {
    let mut config = amp_config();
    let driver = std::sync::Arc::new(GenericConfigDriver::default());

    let changed = prompt::with_hosted_driver(driver.clone(), || {
        configure_generic_config_options_for_init(
            &mut config,
            &response_with_typed_and_generic_options(),
            true,
        )
    })
    .expect("generic options persist");

    assert!(changed);
    assert_eq!(
        driver
            .offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_slice(),
        ["agent.persona", "fast"]
    );
    assert_eq!(
        config.agent.config_options.get("agent.persona"),
        Some(&crate::config::AgentConfigOptionValue::Text(
            "research".to_owned()
        ))
    );
    assert_eq!(
        config.agent.config_options.get("fast"),
        Some(&crate::config::AgentConfigOptionValue::Bool(true))
    );
    let canonical = config.to_canonical_toml().expect("canonical config");
    let reloaded = crate::config::load_config_from_str(&canonical).expect("reload config");
    assert_eq!(reloaded.agent.config_options, config.agent.config_options);
}

#[test]
fn skipped_generic_options_keep_the_advertised_defaults_unoverridden() {
    let mut config = amp_config();
    let driver = std::sync::Arc::new(GenericConfigDriver {
        skip_all: true,
        ..GenericConfigDriver::default()
    });

    let changed = prompt::with_hosted_driver(driver.clone(), || {
        configure_generic_config_options_for_init(
            &mut config,
            &response_with_typed_and_generic_options(),
            true,
        )
    })
    .expect("skipped options retain defaults");

    assert!(!changed);
    assert!(config.agent.config_options.is_empty());
    assert_eq!(
        driver
            .offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_slice(),
        ["agent.persona", "fast"]
    );
}

#[test]
fn deferred_mapped_credential_writes_explicit_model_without_discovery() {
    // Holds the discovery-fixture env lock so a sibling test's fixture path is never observed.
    #[cfg(feature = "test-fixtures")]
    let _env = crate::cli::init::test_env::TestEnvGuard::set(&[]);
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

/// Picks `openrouter/model-b` at the model prompt and, at that moment, rewrites the discovery
/// fixture so only the post-model advertisement carries efforts; then takes the first effort.
struct ModelDependentEffortDriver {
    fixture_path: PathBuf,
    offered: std::sync::Mutex<Vec<(prompt::HostedPromptKind, Vec<String>)>>,
}

fn write_discovery_fixture(path: &Path, efforts: &[&str]) {
    let mut options = vec![serde_json::json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": "openrouter/model-a",
        "options": [
            { "value": "openrouter/model-a", "name": "openrouter/model-a" },
            { "value": "openrouter/model-b", "name": "openrouter/model-b" }
        ]
    })];
    if !efforts.is_empty() {
        options.push(serde_json::json!({
            "id": "effort",
            "name": "Effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": efforts[0],
            "options": efforts
                .iter()
                .map(|value| serde_json::json!({ "value": value, "name": value }))
                .collect::<Vec<_>>()
        }));
    }
    std::fs::write(path, serde_json::Value::Array(options).to_string()).expect("write fixture");
}

impl prompt::HostedPromptDriver for ModelDependentEffortDriver {
    fn select(
        &self,
        request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
        let values: Vec<String> = request
            .items
            .iter()
            .map(|item| item.value.clone())
            .collect();
        let choice = match request.kind {
            prompt::HostedPromptKind::Model => {
                write_discovery_fixture(&self.fixture_path, &["high"]);
                values
                    .iter()
                    .position(|value| value == "openrouter/model-b")
            }
            prompt::HostedPromptKind::Effort => Some(0),
            _ => None,
        };
        self.offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((request.kind, values));
        Ok(prompt::HostedPromptOutcome::Handled(choice))
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

#[cfg(feature = "test-fixtures")]
#[test]
fn effort_prompt_reads_the_advertisement_of_the_model_just_picked() {
    let home = tempfile::tempdir().expect("tempdir");
    let fixture_path = home.path().join("config-options.json");
    write_discovery_fixture(&fixture_path, &[]);
    let _env = crate::cli::init::test_env::TestEnvGuard::set(&[
        (FIXTURE_CONFIG_OPTIONS_ENV, fixture_path.as_path()),
        (
            crate::dev_gates::PROVIDER_MODELS_BASE_ENV,
            Path::new("http://127.0.0.1:1"),
        ),
    ]);
    let mut store = crate::secrets::SecretStore::open_or_create(home.path()).expect("secret store");
    store
        .set_many([("OPENROUTER_API_KEY", "test-openrouter-key")])
        .expect("seed key");
    let secrets = crate::secrets::new_shared_secret_store(store);
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
    let args = parse_init_args(&[]);
    let driver = std::sync::Arc::new(ModelDependentEffortDriver {
        fixture_path: fixture_path.clone(),
        offered: std::sync::Mutex::new(Vec::new()),
    });

    let outcome = prompt::with_hosted_driver(driver.clone(), || {
        configure_model_and_mode_for_init(
            &args,
            home.path(),
            &registry,
            &mut config,
            Path::new("acps-config.toml"),
            &secrets,
        )
    })
    .expect("interactive discovery succeeds");

    assert_eq!(outcome.model_action, ModelModeAction::Set);
    assert_eq!(
        config
            .agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref()),
        Some("openrouter/model-b")
    );
    assert_eq!(
        outcome.effort_action,
        ModelModeAction::Set,
        "the effort lane must read the post-model advertisement"
    );
    assert_eq!(config.agent.effort.as_deref(), Some("high"));
    let offered = driver
        .offered
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        offered.iter().any(|(kind, values)| {
            matches!(kind, prompt::HostedPromptKind::Effort) && values.contains(&"high".to_owned())
        }),
        "effort prompt was not offered the refreshed values: {offered:?}"
    );
}

/// Goose cannot answer `session/new` before `GOOSE_MODEL` is configured, so its model lane reads
/// the provider catalog and every spawning lane waits for the model to settle.
fn goose_config(provider_model: Option<&str>) -> Config {
    let mut config = crate::config::load_config_from_str(include_str!(
        "../../../../tests/fixtures/valid-opencode-stack.toml"
    ))
    .expect("fixture config");
    config.agent.id = "goose".to_owned();
    config.agent.name = "Goose".to_owned();
    config.agent.command = "goose".to_owned();
    config.agent.env = vec!["OPENROUTER_API_KEY".to_owned()];
    config.agent.provider = Some(crate::config::AgentProviderConfig {
        id: "openrouter".to_owned(),
        model: provider_model.map(str::to_owned),
        api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
        custom: None,
    });
    config
}

/// Only the fixture-gated catalog test seeds a catalog, so the default-feature build has no caller.
#[cfg(feature = "test-fixtures")]
fn seed_provider_catalog(home: &Path, models: &[&str]) {
    let path = crate::runtime::agent::provider_model_catalog::cache_path(home);
    std::fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let entries: Vec<serde_json::Value> = models
        .iter()
        .map(|value| serde_json::json!({ "value": value }))
        .collect();
    std::fs::write(
        &path,
        serde_json::json!({
            "version": 2,
            "providers": { "openrouter": { "fetched_at": fetched_at, "models": entries } }
        })
        .to_string(),
    )
    .expect("write cache");
}

fn configured_provider_model(config: &Config) -> Option<&str> {
    config
        .agent
        .provider
        .as_ref()
        .and_then(|provider| provider.model.as_deref())
}

#[test]
fn goose_explicit_model_is_written_without_a_discovery_session() {
    let home = tempfile::tempdir().expect("tempdir");
    let secrets = crate::secrets::new_shared_secret_store(
        crate::secrets::SecretStore::open_or_create(home.path()).expect("secret store"),
    );
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let mut config = goose_config(None);
    let args = parse_init_args(&["--model", "openrouter/model-x", "--non-interactive"]);

    // No catalog, no credential, and no goose binary: an explicit model must still land.
    let outcome = configure_model_and_mode_for_init(
        &args,
        home.path(),
        &registry,
        &mut config,
        Path::new("acps-config.toml"),
        &secrets,
    )
    .expect("an explicit model needs no discovery");

    assert_eq!(outcome.model_action, ModelModeAction::Set);
    assert!(!outcome.acp_verified, "no session may have been opened");
    assert_eq!(
        configured_provider_model(&config),
        Some("openrouter/model-x")
    );
}

#[test]
fn goose_rejects_an_empty_explicit_model() {
    let home = tempfile::tempdir().expect("tempdir");
    let secrets = crate::secrets::new_shared_secret_store(
        crate::secrets::SecretStore::open_or_create(home.path()).expect("secret store"),
    );
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let mut config = goose_config(None);
    let args = parse_init_args(&["--model", "   ", "--non-interactive"]);

    let error = configure_model_and_mode_for_init(
        &args,
        home.path(),
        &registry,
        &mut config,
        Path::new("acps-config.toml"),
        &secrets,
    )
    .expect_err("an empty model id is not a model");

    assert!(error.to_string().contains("non-empty model id"), "{error}");
    assert_eq!(configured_provider_model(&config), None);
}

#[test]
fn goose_mode_and_effort_flags_are_rejected_without_a_model() {
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let config = goose_config(None);

    for flag in ["--mode", "--effort"] {
        let args = parse_init_args(&[flag, "some-value"]);
        let error = preflight_model_and_mode_for_init(
            &args,
            &registry,
            &config,
            Path::new("acps-config.toml"),
        )
        .expect_err("discovery needs a model first");
        assert!(
            error.to_string().contains("needs a configured model"),
            "{flag}: {error}"
        );
    }

    let args = parse_init_args(&["--mode", "some-value"]);
    preflight_model_and_mode_for_init(
        &args,
        &registry,
        &goose_config(Some("openrouter/model-a")),
        Path::new("acps-config.toml"),
    )
    .expect("a configured model unblocks the mode lane");
}

#[cfg(feature = "test-fixtures")]
#[test]
fn goose_model_picker_offers_the_provider_catalog_and_unblocks_the_mode_lane() {
    let home = tempfile::tempdir().expect("tempdir");
    let fixture_path = home.path().join("config-options.json");
    // The advertisement carries models goose could only offer once it is running; the catalog is
    // the truthful pickable set, so these must never reach the model prompt.
    std::fs::write(
        &fixture_path,
        serde_json::to_string(
            &response_with(&["advertised/model"], &["auto", "chat"]).config_options,
        )
        .expect("fixture serializes"),
    )
    .expect("write fixture");
    let _env = crate::cli::init::test_env::TestEnvGuard::set(&[(
        FIXTURE_CONFIG_OPTIONS_ENV,
        fixture_path.as_path(),
    )]);
    seed_provider_catalog(home.path(), &["openrouter/model-a", "openrouter/model-b"]);
    let secrets = crate::secrets::new_shared_secret_store(
        crate::secrets::SecretStore::open_or_create(home.path()).expect("secret store"),
    );
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let mut config = goose_config(None);
    let args = parse_init_args(&[]);
    let driver = std::sync::Arc::new(FirstChoiceDriver::default());

    let outcome = prompt::with_hosted_driver(driver.clone(), || {
        configure_model_and_mode_for_init(
            &args,
            home.path(),
            &registry,
            &mut config,
            Path::new("acps-config.toml"),
            &secrets,
        )
    })
    .expect("the catalog lane settles the model");

    assert_eq!(outcome.model_action, ModelModeAction::Set);
    assert_eq!(
        configured_provider_model(&config),
        Some("openrouter/model-a")
    );
    assert_eq!(
        driver.offered_for(prompt::HostedPromptKind::Model),
        vec![vec![
            "openrouter/model-a".to_owned(),
            "openrouter/model-b".to_owned(),
            "__skip".to_owned(),
        ]],
        "the model prompt must offer catalog values, never the advertisement"
    );
    assert!(
        driver
            .offered_for(prompt::HostedPromptKind::Mode)
            .iter()
            .any(|values| values.contains(&"chat".to_owned())),
        "the mode lane runs once the model is configured",
    );
}

#[cfg(feature = "test-fixtures")]
#[test]
fn goose_mode_and_effort_lanes_are_skipped_while_no_model_is_configured() {
    let home = tempfile::tempdir().expect("tempdir");
    let fixture_path = home.path().join("config-options.json");
    std::fs::write(
        &fixture_path,
        serde_json::to_string(
            &response_with(&["advertised/model"], &["auto", "chat"]).config_options,
        )
        .expect("fixture serializes"),
    )
    .expect("write fixture");
    let _env = crate::cli::init::test_env::TestEnvGuard::set(&[(
        FIXTURE_CONFIG_OPTIONS_ENV,
        fixture_path.as_path(),
    )]);
    // No catalog is seeded, so the model lane has nothing to offer and skips.
    let secrets = crate::secrets::new_shared_secret_store(
        crate::secrets::SecretStore::open_or_create(home.path()).expect("secret store"),
    );
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let mut config = goose_config(None);
    let args = parse_init_args(&[]);
    let driver = std::sync::Arc::new(FirstChoiceDriver::default());

    let outcome = prompt::with_hosted_driver(driver.clone(), || {
        configure_model_and_mode_for_init(
            &args,
            home.path(),
            &registry,
            &mut config,
            Path::new("acps-config.toml"),
            &secrets,
        )
    })
    .expect("an unconfigured model is not an init failure");

    assert_eq!(outcome.model_action, ModelModeAction::Skipped);
    assert_eq!(outcome.mode_action, ModelModeAction::Skipped);
    assert_eq!(outcome.effort_action, ModelModeAction::Skipped);
    assert!(!outcome.acp_verified, "no session may have been opened");
    assert!(config.agent.mode.is_none());
    assert!(
        driver
            .offered_for(prompt::HostedPromptKind::Mode)
            .is_empty(),
        "the mode prompt must not run off an advertisement no session could produce",
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
