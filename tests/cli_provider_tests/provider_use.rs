use acp_stack::secrets::SecretStore;
use serde_json::Value;
use std::fs;

use crate::common::cli::*;

fn seed_flat_secrets(home: &std::path::Path, env_names: &[&str]) {
    let mut store = SecretStore::open_or_create(home).expect("secret store should open");
    let values = env_names
        .iter()
        .map(|name| ((*name).to_owned(), format!("test-{name}")))
        .collect::<Vec<_>>();
    store
        .set_many(
            values
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .expect("flat test secrets should be stored");
}

#[test]
fn agent_provider_use_updates_config_and_generated_opencode_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("target: opencode"))
        .stdout(predicates::str::contains("provider: openai"))
        .stdout(predicates::str::contains("restart the supervised agent"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "openai""#));
    assert!(config.contains(r#"model = "openai/gpt-5.5""#));
    assert!(!config.contains(r#"api_key_ref = "OPENAI_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "openai/gpt-5.5");
    assert_eq!(
        opencode["provider"]["openai"]["options"]["apiKey"],
        "{env:OPENAI_API_KEY}"
    );
}

#[test]
fn agent_provider_use_uses_agent_native_provider_id_for_collapsed_provider() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(tempdir.path(), "vercel-ai-gateway", &["AI_GATEWAY_API_KEY"]);
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(tempdir.path(), &["vercel/test-model"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "vercel-ai-gateway",
            "--model",
            "test-model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: vercel-ai-gateway"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "vercel-ai-gateway""#));
    assert!(config.contains(r#"model = "vercel/test-model""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "vercel/test-model");
    assert_eq!(
        opencode["provider"]["vercel"]["options"]["apiKey"],
        "{env:AI_GATEWAY_API_KEY}"
    );
    assert!(opencode["provider"]["vercel-ai-gateway"].is_null());
}

#[test]
fn agent_set_custom_opencode_provider_writes_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--model-name",
            "My Model",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("api_key_ref: CUSTOM_API_KEY"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "myprovider""#));
    assert!(config.contains(r#"api_key_ref = "CUSTOM_API_KEY""#));
    assert!(config.contains("[array.targets.agent.provider.custom]"));
    assert!(config.contains(r#"context = 200000"#));
    assert!(config.contains(r#"output_max_tokens = 65536"#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(opencode["model"], "my-model");
    assert_eq!(
        opencode["provider"]["myprovider"]["options"]["apiKey"],
        "{env:CUSTOM_API_KEY}"
    );
    assert_eq!(
        opencode["provider"]["myprovider"]["models"]["my-model"]["limit"]["context"],
        200000
    );
}

#[test]
fn agent_set_custom_provider_rejects_comma_token_limits() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "set",
            "--custom-provider",
            "--provider",
            "myprovider",
            "--provider-name",
            "My Provider",
            "--base-url",
            "https://api.myprovider.example/v1",
            "--api-key-ref",
            "CUSTOM_API_KEY",
            "--model",
            "my-model",
            "--context",
            "200,000",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "must be a plain integer without commas",
        ));
}

#[test]
fn agent_provider_use_goose_provider_updates_generated_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "goose""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Goose""#)
        .replace(r#"command = "opencode""#, r#"command = "goose""#)
        .replace(
            r#"env = ["OPENCODE_API_KEY"]"#,
            r#"env = ["OPENROUTER_API_KEY"]"#,
        )
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    seed_provider_credential(tempdir.path(), "openrouter", &["OPENROUTER_API_KEY"]);
    let options_path =
        write_acp_config_options(tempdir.path(), &["deepseek/deepseek-v4-flash"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openrouter",
            "--model",
            "deepseek/deepseek-v4-flash",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: openrouter"))
        .stdout(predicates::str::contains(
            "switched live via ACP session/set_config_option",
        ));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "openrouter""#));
    assert!(config.contains(r#"model = "deepseek/deepseek-v4-flash""#));
    assert!(!config.contains(r#"api_key_ref = "OPENROUTER_API_KEY""#));

    let goose_path = tempdir
        .path()
        .join(".config")
        .join("goose")
        .join("config.yaml");
    let goose: serde_norway::Value = serde_norway::from_str(
        &fs::read_to_string(goose_path).expect("goose config should be readable"),
    )
    .expect("goose config should parse");
    assert_eq!(goose["GOOSE_PROVIDER"], "openrouter");
    assert_eq!(goose["GOOSE_MODEL"], "deepseek/deepseek-v4-flash");
}

#[test]
fn agent_provider_use_rejects_api_key_ref_argument() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), codex_config())
        .expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "gpt-5.5",
            "--api-key-ref",
            "OPENAI_API_KEY",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "unexpected argument '--api-key-ref'",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_provider_use_rejects_provider_not_supported_by_agent() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "azure-openai-responses",
            "--model",
            "azure-openai-responses/test-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `azure-openai-responses` is not supported for agent `opencode`",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!after.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_provider_use_rejects_providers_without_api_key_mapping() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "google-vertex",
            "--model",
            "google-vertex/test-model",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provider `google-vertex` does not use an acps-managed API key",
        ));

    let after =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!after.contains("[array.targets.agent.provider]"));
}

#[test]
fn agent_provider_use_resolves_cloudflare_companion_fields() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "pi""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Pi Agent""#)
        .replace(r#"command = "opencode""#, r#"command = "pi-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    seed_provider_credential(
        tempdir.path(),
        "cloudflare-ai-gateway",
        &[
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_GATEWAY_ID",
        ],
    );
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "cloudflare-ai-gateway",
            "--model",
            "workers-ai/@cf/moonshotai/kimi-k2.6",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: cloudflare-ai-gateway"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(config.contains(r#"id = "cloudflare-ai-gateway""#));
    assert!(config.contains(r#"model = "workers-ai/@cf/moonshotai/kimi-k2.6""#));
    assert!(!config.contains(r#"api_key_ref = "CLOUDFLARE_API_KEY""#));
}

#[test]
fn agent_provider_use_opencode_cloudflare_gateway_uses_canonical_token_env() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(
        tempdir.path(),
        "cloudflare-ai-gateway",
        &[
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_GATEWAY_ID",
        ],
    );
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "cloudflare-ai-gateway",
            "--model",
            "cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: cloudflare-ai-gateway"));

    let config = fs::read_to_string(config_dir.join("acps-config.toml"))
        .expect("updated config should be readable");
    assert!(!config.contains(r#"api_key_ref = "CLOUDFLARE_API_TOKEN""#));
    assert!(!config.contains(r#""CLOUDFLARE_API_KEY""#));

    let opencode_path = tempdir
        .path()
        .join(".config")
        .join("opencode")
        .join("opencode.json");
    let opencode: Value = serde_json::from_str(
        &fs::read_to_string(opencode_path).expect("opencode config should be readable"),
    )
    .expect("opencode config should parse");
    assert_eq!(
        opencode["model"],
        "cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6"
    );
    assert_eq!(
        opencode["provider"]["cloudflare-ai-gateway"]["options"]["apiKey"],
        "{env:CLOUDFLARE_API_TOKEN}"
    );
}

#[test]
fn agent_provider_use_without_model_selects_provider_without_model() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(
        tempdir.path(),
        "cloudflare-workers-ai",
        &["CLOUDFLARE_API_KEY", "CLOUDFLARE_ACCOUNT_ID"],
    );
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(
        tempdir.path(),
        &["cloudflare-workers-ai/@cf/moonshotai/kimi-k2.6"],
        &[],
    );

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args(["agent", "provider", "use", "cloudflare-workers-ai"])
        .assert()
        .success()
        .stdout(predicates::str::contains("provider: cloudflare-workers-ai"));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(config.contains("[array.targets.agent.provider]"));
    assert!(config.contains(r#"id = "cloudflare-workers-ai""#));
    let parsed: toml::Value = toml::from_str(&config).expect("config should parse");
    assert!(
        primary_array_agent_value(&parsed)["provider"]
            .get("model")
            .is_none()
    );
}

#[test]
fn agent_provider_use_does_not_partially_write_main_config_when_provisioning_fails() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);
    seed_flat_secrets(tempdir.path(), &["OPENCODE_API_KEY"]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);
    let opencode_dir = tempdir.path().join(".config").join("opencode");
    fs::create_dir_all(&opencode_dir).expect("opencode config dir should be created");
    fs::write(opencode_dir.join("opencode.json"), "[]")
        .expect("invalid opencode config should be written");

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "existing JSON root must be an object",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[array.targets.agent.provider]"));
    assert!(!config.contains(r#""OPENAI_API_KEY""#));
}

#[test]
fn agent_provider_use_validates_model_against_acp_config_options() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    fs::write(config_dir.join("acps-config.toml"), VALID_CONFIG).expect("config should be written");
    seed_provider_credential(tempdir.path(), "openai", &["OPENAI_API_KEY"]);
    let options_path = write_acp_config_options(tempdir.path(), &["openai/gpt-5.5"], &[]);

    acps_command()
        .env("HOME", tempdir.path())
        .env("ACP_STACK_AGENT_CONFIG_OPTIONS_PATH", &options_path)
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "openai/not-advertised",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent did not advertise `openai/not-advertised` as an available `model`",
        ));

    let config =
        fs::read_to_string(config_dir.join("acps-config.toml")).expect("config should be readable");
    assert!(!config.contains("[array.targets.agent.provider]"));
    assert!(
        !tempdir
            .path()
            .join(".config/opencode/opencode.json")
            .exists(),
        "failed discovery must restore the prior OpenCode config state"
    );
}

#[test]
fn agent_provider_use_amp_rejects_provider_model_settings() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let config_dir = tempdir.path().join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir should be created");
    let config = VALID_CONFIG
        .replace(r#"id = "opencode""#, r#"id = "amp""#)
        .replace(r#"name = "OpenCode""#, r#"name = "Amp Code""#)
        .replace(r#"command = "opencode""#, r#"command = "amp-acp""#)
        .replace(r#"args = ["acp"]"#, r#"args = []"#)
        .replace(r#"env = ["OPENCODE_API_KEY"]"#, r#"env = ["AMP_API_KEY"]"#)
        .replace(
            r#"
[agent.install]
type = "shell"
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
"#,
            "",
        );
    fs::write(config_dir.join("acps-config.toml"), config).expect("config should be written");
    SecretStore::open_or_create(tempdir.path()).expect("secret store should open");

    acps_command()
        .env("HOME", tempdir.path())
        .args([
            "agent",
            "provider",
            "use",
            "openai",
            "--model",
            "openai/gpt-5.5",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Amp Code does not support mapped provider selection",
        ));
}
