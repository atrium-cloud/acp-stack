use super::*;

pub(super) const ANTIGRAVITY_AGENT_ID: &str = "antigravity";
const ANTIGRAVITY_SETTINGS_DIR: &str = "antigravity-acp";
const ANTIGRAVITY_SETTINGS_FILE: &str = "settings.json";
const ANTIGRAVITY_AUTH_KEY: &str = "auth";
const ANTIGRAVITY_AUTH_TYPE_KEY: &str = "type";
const ANTIGRAVITY_AUTH_TYPE_VALUE: &str = "gemini-api-key";

fn antigravity_settings_path(home: &Path) -> PathBuf {
    home.join(".gemini")
        .join(ANTIGRAVITY_SETTINGS_DIR)
        .join(ANTIGRAVITY_SETTINGS_FILE)
}

/// The ACP server reads `~/.gemini/antigravity-acp/settings.json` — a
/// different file and shape from the interactive CLI's
/// `antigravity-cli/settings.json` (`modelProvider`) documented for `agy`.
/// Headless auth needs `auth.type = "gemini-api-key"` here plus
/// `GEMINI_API_KEY` in the process env; without both, `session/new` is
/// rejected with "Authentication required" and the alternatives are browser
/// OAuth or GCP credentials, which a headless runtime cannot complete.
pub(super) fn provision_antigravity_config(_config: &Config, home: &Path) -> Result<Vec<PathBuf>> {
    let path = antigravity_settings_path(home);
    let mut root = read_json_object(&path)?;
    let auth = root
        .entry(ANTIGRAVITY_AUTH_KEY.to_owned())
        .or_insert_with(|| json!({}));
    if !auth.is_object() {
        *auth = json!({});
    }
    if let Some(auth) = auth.as_object_mut() {
        auth.insert(
            ANTIGRAVITY_AUTH_TYPE_KEY.to_owned(),
            json!(ANTIGRAVITY_AUTH_TYPE_VALUE),
        );
    }
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
    // Only the managed value is removed; an operator-set auth type is not
    // acps state to clean up.
    let managed = root
        .get(ANTIGRAVITY_AUTH_KEY)
        .and_then(|auth| auth.get(ANTIGRAVITY_AUTH_TYPE_KEY))
        == Some(&json!(ANTIGRAVITY_AUTH_TYPE_VALUE));
    if managed {
        if let Some(auth) = root
            .get_mut(ANTIGRAVITY_AUTH_KEY)
            .and_then(|auth| auth.as_object_mut())
        {
            auth.remove(ANTIGRAVITY_AUTH_TYPE_KEY);
            if auth.is_empty() {
                root.remove(ANTIGRAVITY_AUTH_KEY);
            }
        }
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
    fn antigravity_provision_writes_the_auth_type_key() {
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
        assert_eq!(value["auth"]["type"], "gemini-api-key");
    }

    #[test]
    fn antigravity_provision_preserves_operator_keys_and_is_a_fixed_point() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = antigravity_settings_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "{\n  \"telemetry\": false,\n  \"auth\": {\"custom\": true}\n}\n",
        )
        .expect("write existing settings");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        provision_agent_headless_config(&config, tempdir.path()).expect("first provision");
        let first = std::fs::read(&path).expect("first settings readable");
        provision_agent_headless_config(&config, tempdir.path()).expect("second provision");
        let second = std::fs::read(&path).expect("second settings readable");

        assert_eq!(first, second, "re-provision must be a fixed point");
        let value = antigravity_settings_value(tempdir.path());
        assert_eq!(value["auth"]["type"], "gemini-api-key");
        assert_eq!(value["auth"]["custom"], true);
        assert_eq!(value["telemetry"], false);
    }

    #[test]
    fn antigravity_cleanup_removes_only_the_managed_key() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = antigravity_settings_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            "{\n  \"auth\": {\"type\": \"gemini-api-key\", \"custom\": true},\n  \"telemetry\": false\n}\n",
        )
        .expect("write existing settings");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0].label, "Antigravity settings");
        let value = antigravity_settings_value(tempdir.path());
        assert!(value["auth"].get("type").is_none(), "{value:?}");
        assert_eq!(value["auth"]["custom"], true);
        assert_eq!(value["telemetry"], false);
    }

    #[test]
    fn antigravity_cleanup_removes_a_fully_managed_file() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = antigravity_settings_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, "{\n  \"auth\": {\"type\": \"gemini-api-key\"}\n}\n")
            .expect("write existing settings");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        assert!(!path.exists());
    }

    #[test]
    fn antigravity_cleanup_leaves_an_operator_set_auth_type() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = antigravity_settings_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(&path, "{\n  \"auth\": {\"type\": \"oauth-personal\"}\n}\n")
            .expect("write existing settings");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert!(cleaned.is_empty());
        let value = antigravity_settings_value(tempdir.path());
        assert_eq!(value["auth"]["type"], "oauth-personal");
    }

    #[test]
    fn antigravity_cleanup_without_settings_file_is_a_no_op() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        let cleaned = cleanup_agent_headless_config(&config, tempdir.path()).expect("cleanup");

        assert!(cleaned.is_empty());
    }

    /// The daemon-path verification run (Sprite VM, 2026-08-21) passed with a
    /// hand-written settings file; this pins the provisioner to that exact
    /// managed shape so the passing run transfers to the provisioned path.
    #[test]
    fn antigravity_provision_matches_the_verified_settings_shape() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let config = config_with_agent("antigravity", &["GEMINI_API_KEY"]);

        provision_agent_headless_config(&config, tempdir.path()).expect("provision");

        let value = antigravity_settings_value(tempdir.path());
        assert_eq!(
            value,
            serde_json::json!({"auth": {"type": "gemini-api-key"}})
        );
    }
}
