use super::*;
use crate::runtime::agent::provider_keys::providers_for_agent;

/// Kilo keeps provider selection itself (`set_provider = false`), so the only acps-written key in
/// its config is the endpoint override: `provider.<native>.options.baseURL` for the provider the
/// override names. Every other override-capable provider's `baseURL` is dropped on each write so a
/// cleared or moved override restores the vendor endpoint.
pub(super) fn provision_kilo_config(
    home: &Path,
    endpoint: Option<&crate::secrets::ProviderEndpointOverride>,
) -> Result<Vec<PathBuf>> {
    let path = kilo_config_path(home);
    let rerouted = match endpoint {
        Some(endpoint) => {
            let native_provider_id =
                agent_provider_id_for_provider_id(KILO_AGENT_ID, &endpoint.provider_id)
                    .ok_or_else(|| StackError::AgentConfigProvision {
                        path: path.clone(),
                        reason: format!(
                            "kilo provider `{}` has no native provider id in provider/env mapping",
                            endpoint.provider_id
                        ),
                    })?;
            let base_url = super::rerouted_mapped_base_url_for(
                Some(endpoint),
                KILO_AGENT_ID,
                &endpoint.provider_id,
                &path,
            )?;
            base_url.map(|base_url| (native_provider_id, base_url))
        }
        None => None,
    };
    if rerouted.is_none() && !path.exists() {
        return Ok(Vec::new());
    }
    let mut root = read_json_object(&path)?;
    let changed = remove_managed_kilo_base_urls(&mut root);
    let Some((native_provider_id, base_url)) = rerouted else {
        if changed {
            write_or_remove_json_object(&path, root)?;
            return Ok(vec![path]);
        }
        return Ok(Vec::new());
    };
    let providers = ensure_object_field(&mut root, "provider", &path)?;
    let provider_config = ensure_object_field(providers, native_provider_id, &path)?;
    let options = ensure_object_field(provider_config, "options", &path)?;
    options.insert("baseURL".to_owned(), json!(base_url));
    write_json_object(&path, root)?;
    Ok(vec![path])
}

pub(super) fn cleanup_kilo_config(home: &Path) -> Result<Vec<CleanedAgentConfig>> {
    let path = kilo_config_path(home);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut root = read_json_object(&path)?;
    if !remove_managed_kilo_base_urls(&mut root) {
        return Ok(Vec::new());
    }
    write_or_remove_json_object(&path, root)?;
    Ok(vec![CleanedAgentConfig {
        label: "Kilo config",
        path,
    }])
}

fn kilo_config_path(home: &Path) -> PathBuf {
    home.join(".config").join("kilo").join("kilo.json")
}

/// Drop `options.baseURL` from every provider acps can reroute under kilo, pruning the objects
/// that end up empty. Providers acps cannot reroute are an operator's own and stay untouched.
fn remove_managed_kilo_base_urls(root: &mut Map<String, serde_json::Value>) -> bool {
    let mut changed = false;
    let mut remove_provider_object = false;
    if let Some(providers) = root
        .get_mut("provider")
        .and_then(serde_json::Value::as_object_mut)
    {
        for summary in providers_for_agent(KILO_AGENT_ID) {
            let native_provider_id = summary.agent_provider_id.unwrap_or(summary.id);
            let Some(provider_config) = providers
                .get_mut(native_provider_id)
                .and_then(serde_json::Value::as_object_mut)
            else {
                continue;
            };
            let mut remove_options = false;
            if let Some(options) = provider_config
                .get_mut("options")
                .and_then(serde_json::Value::as_object_mut)
            {
                changed |= options.remove("baseURL").is_some();
                remove_options = options.is_empty();
            }
            if remove_options {
                provider_config.remove("options");
            }
            if provider_config.is_empty() {
                providers.remove(native_provider_id);
            }
        }
        remove_provider_object = providers.is_empty();
    }
    if remove_provider_object {
        root.remove("provider");
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn kilo_endpoint(provider_id: &str) -> crate::secrets::ProviderEndpointOverride {
        crate::secrets::ProviderEndpointOverride {
            provider_id: provider_id.to_owned(),
            base_url: "http://127.0.0.1:3129".to_owned(),
            companion_values: std::collections::BTreeMap::new(),
        }
    }

    fn kilo_config_value(home: &Path) -> Option<Value> {
        std::fs::read_to_string(kilo_config_path(home))
            .ok()
            .map(|text| serde_json::from_str(&text).expect("kilo config json parses"))
    }

    #[test]
    fn kilo_writes_nothing_without_an_override() {
        let tempdir = tempfile::tempdir().expect("tempdir");

        let written = provision_kilo_config(tempdir.path(), None).expect("provision");

        assert!(written.is_empty());
        assert!(kilo_config_value(tempdir.path()).is_none());
    }

    #[test]
    fn kilo_gateway_endpoint_keeps_the_vendor_path_and_is_restored() {
        let tempdir = tempfile::tempdir().expect("tempdir");

        provision_kilo_config(tempdir.path(), Some(&kilo_endpoint("kilo")))
            .expect("provision with override");
        let value = kilo_config_value(tempdir.path()).expect("kilo config written");
        assert_eq!(
            value["provider"]["kilo"]["options"]["baseURL"],
            "http://127.0.0.1:3129/api/gateway"
        );

        provision_kilo_config(tempdir.path(), None).expect("provision without");
        assert!(kilo_config_value(tempdir.path()).is_none());
    }

    #[test]
    fn kilo_leaves_operator_keys_alone_when_clearing() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = kilo_config_path(tempdir.path());
        std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent");
        std::fs::write(
            &path,
            r#"{"model":"kilo/claude","provider":{"kilo":{"options":{"baseURL":"http://127.0.0.1:3129/api/gateway","timeout":5}},"other":{"options":{"baseURL":"https://operator.example/v1"}}}}"#,
        )
        .expect("write kilo config");

        provision_kilo_config(tempdir.path(), None).expect("provision without");

        let value = kilo_config_value(tempdir.path()).expect("kilo config kept");
        assert_eq!(value["model"], "kilo/claude");
        assert!(value["provider"]["kilo"]["options"]["baseURL"].is_null());
        assert_eq!(value["provider"]["kilo"]["options"]["timeout"], 5);
        assert_eq!(
            value["provider"]["other"]["options"]["baseURL"],
            "https://operator.example/v1"
        );
    }

    #[test]
    fn kilo_openrouter_endpoint_keeps_the_vendor_path() {
        let tempdir = tempfile::tempdir().expect("tempdir");

        provision_kilo_config(tempdir.path(), Some(&kilo_endpoint("openrouter")))
            .expect("provision with override");

        let value = kilo_config_value(tempdir.path()).expect("kilo config written");
        assert_eq!(
            value["provider"]["openrouter"]["options"]["baseURL"],
            "http://127.0.0.1:3129/api/v1"
        );
    }

    #[test]
    fn kilo_refuses_a_provider_it_does_not_map() {
        let tempdir = tempfile::tempdir().expect("tempdir");

        let error = provision_kilo_config(tempdir.path(), Some(&kilo_endpoint("cerebras")))
            .expect_err("unmapped provider must refuse");

        assert!(error.to_string().contains("cerebras"), "{error}");
    }

    #[test]
    fn kilo_cleanup_removes_the_managed_base_url() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        provision_kilo_config(tempdir.path(), Some(&kilo_endpoint("kilo")))
            .expect("provision with override");

        let cleaned = cleanup_kilo_config(tempdir.path()).expect("cleanup");

        assert_eq!(cleaned.len(), 1);
        assert!(kilo_config_value(tempdir.path()).is_none());
    }
}
