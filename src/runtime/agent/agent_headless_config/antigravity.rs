use super::*;

pub(super) const ANTIGRAVITY_AGENT_ID: &str = "antigravity";
const ANTIGRAVITY_SETTINGS_DIR: &str = "antigravity-cli";
const ANTIGRAVITY_SETTINGS_FILE: &str = "settings.json";
const ANTIGRAVITY_MODEL_PROVIDER_KEY: &str = "modelProvider";
const ANTIGRAVITY_MODEL_PROVIDER_VALUE: &str = "gemini";

fn antigravity_settings_path(home: &Path) -> PathBuf {
    home.join(".gemini")
        .join(ANTIGRAVITY_SETTINGS_DIR)
        .join(ANTIGRAVITY_SETTINGS_FILE)
}

/// Antigravity's API-key mode requires `modelProvider: "gemini"` in its
/// settings file in addition to `GEMINI_API_KEY` in the process env; without
/// the key the harness tries a browser sign-in, which a headless runtime
/// cannot complete. There is no provider selection — `gemini` is the sole
/// documented value — so the managed key is written unconditionally.
pub(super) fn provision_antigravity_config(_config: &Config, home: &Path) -> Result<Vec<PathBuf>> {
    let path = antigravity_settings_path(home);
    let mut root = read_json_object(&path)?;
    root.insert(
        ANTIGRAVITY_MODEL_PROVIDER_KEY.to_owned(),
        json!(ANTIGRAVITY_MODEL_PROVIDER_VALUE),
    );
    write_json_object(&path, root)?;
    Ok(vec![path])
}

pub(super) fn cleanup_antigravity_config(
    _config: &Config,
    home: &Path,
) -> Result<Vec<CleanedAgentConfig>> {
    let mut cleaned = Vec::new();
    let path = antigravity_settings_path(home);
    if !path.exists() {
        return Ok(cleaned);
    }
    let mut root = read_json_object(&path)?;
    // Only the managed value is removed; an operator-set provider (should
    // upstream ever document another) is not acps state to clean up.
    if root.get(ANTIGRAVITY_MODEL_PROVIDER_KEY) == Some(&json!(ANTIGRAVITY_MODEL_PROVIDER_VALUE)) {
        root.remove(ANTIGRAVITY_MODEL_PROVIDER_KEY);
        write_or_remove_json_object(&path, root)?;
        cleaned.push(CleanedAgentConfig {
            label: "Antigravity settings",
            path,
        });
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn antigravity_settings_value(home: &Path) -> serde_json::Value {
        serde_json::from_str(
            &std::fs::read_to_string(antigravity_settings_path(home))
                .expect("antigravity settings readable"),
        )
        .expect("antigravity settings json parses")
    }

    #[test]
    fn antigravity_provision_writes_the_model_provider_key() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let provisioned =
            provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        assert_eq!(provisioned.len(), 1);
        assert_eq!(
            provisioned[0].path,
            antigravity_settings_path(tempdir.path())
        );
        assert_eq!(provisioned[0].label, "Antigravity settings");
        let value = antigravity_settings_value(tempdir.path());
        assert_eq!(value["modelProvider"], "gemini");
    }

    #[test]
    fn antigravity_provision_preserves_operator_keys_and_is_a_fixed_point() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = antigravity_settings_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, "{\n  \"telemetry\": false\n}\n").expect("write existing settings");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        provision_agent_headless_config(&config, tempdir.path()).expect("first provision");
        let first = std::fs::read(&path).expect("first settings readable");
        provision_agent_headless_config(&config, tempdir.path()).expect("second provision");
        let second = std::fs::read(&path).expect("second settings readable");

        assert_eq!(first, second, "re-provision must be a fixed point");
        let value = antigravity_settings_value(tempdir.path());
        assert_eq!(value["modelProvider"], "gemini");
        assert_eq!(value["telemetry"], false);
    }

    #[test]
    fn antigravity_cleanup_removes_only_the_managed_key() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = antigravity_settings_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "{\n  \"modelProvider\": \"gemini\",\n  \"telemetry\": false\n}\n",
        )
        .expect("write existing settings");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0].label, "Antigravity settings");
        let value = antigravity_settings_value(tempdir.path());
        assert!(value.get("modelProvider").is_none(), "{value:?}");
        assert_eq!(value["telemetry"], false);
    }

    #[test]
    fn antigravity_cleanup_removes_a_fully_managed_file() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = antigravity_settings_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, "{\n  \"modelProvider\": \"gemini\"\n}\n")
            .expect("write existing settings");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        assert!(!path.exists());
    }

    #[test]
    fn antigravity_cleanup_leaves_an_operator_set_provider_value() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = antigravity_settings_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, "{\n  \"modelProvider\": \"something-else\"\n}\n")
            .expect("write existing settings");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert!(cleaned.is_empty());
        let value = antigravity_settings_value(tempdir.path());
        assert_eq!(value["modelProvider"], "something-else");
    }

    #[test]
    fn antigravity_cleanup_without_settings_file_is_a_no_op() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert!(cleaned.is_empty());
    }
}
