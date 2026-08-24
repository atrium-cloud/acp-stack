use super::*;

pub(super) struct PendingInitNativeConfig {
    pub(super) inspected: InspectedNativeConfig,
    pub(super) selection: NativeConfigSelection,
    pub(super) prepared:
        Option<crate::runtime::agent::native_config_import::PreparedNativeConfigImport>,
}

pub(super) fn review_native_config_upload_for_init(
    args: &mut InitArgs,
    config_path: &Path,
) -> Result<Option<PendingInitNativeConfig>> {
    let Some(upload) = args.native_config_upload.take() else {
        return Ok(None);
    };
    if config_path.exists()
        || args.resume
        || args.config_import_source_label().is_some()
        || args.custom_agent_id.is_some()
    {
        return Err(StackError::InvalidParam {
            field: "native_config",
            reason: "hosted native config upload requires a fresh registry-agent init".to_owned(),
        });
    }
    let harness = args
        .agent
        .as_deref()
        .ok_or_else(|| StackError::InvalidParam {
            field: "agent",
            reason: "hosted native config upload requires selecting the agent in the start request"
                .to_owned(),
        })?;
    let inspected =
        inspect_native_config(harness, Some(&upload.filename), upload.content.as_str())?;
    let selection = prompt::native_config_review(inspected.inspection().clone())?;
    validate_native_config_selection(&inspected, &selection)?;
    args.native_config_revision = Some(selection.revision.clone());
    Ok(Some(PendingInitNativeConfig {
        inspected,
        selection,
        prepared: None,
    }))
}

pub(super) fn prepare_native_config_for_new_init(
    args: &InitArgs,
    registry: &RegistryCatalog,
    pending: &mut PendingInitNativeConfig,
    config: &mut Config,
    config_path: &Path,
    home: &Path,
) -> Result<bool> {
    let provider_preapplied = if let Some(provider_id) = args.provider.clone() {
        apply_provider_to_config(args, registry, config, config_path, provider_id)?;
        true
    } else {
        false
    };
    native_config::prepare_for_new_init(pending, config, home)?;
    Ok(provider_preapplied)
}

pub(super) fn configure_subagent_inherit_for_init(
    interactive: bool,
    registry: &RegistryCatalog,
    config: &mut Config,
) -> Result<bool> {
    if config.agent.subagent.is_some() {
        return Ok(false);
    }
    let Some(entry) = registry.lookup(&config.agent.id) else {
        return Ok(false);
    };
    if entry.id != OPENCODE_AGENT_ID || entry.subagent_alias.as_deref() != Some("small_model") {
        return Ok(false);
    }
    let Some(provider) = config.agent.provider.as_ref() else {
        return Ok(false);
    };
    if provider
        .model
        .as_deref()
        .is_none_or(|model| model.trim().is_empty())
    {
        return Ok(false);
    }
    let alias = entry.subagent_alias.as_deref().unwrap_or("subagent");
    // Default-yes: declining leaves `subagent` unset, which means inherit; only an
    // explicit "no" disables it.
    if prompt::confirm(
        prompt::HostedPromptKind::SubagentInheritConfirm,
        interactive,
        &format!("inherit main provider/model for {alias}? declining disables it."),
        true,
    )? {
        return Ok(true);
    }
    config.agent.subagent = Some(AgentSubagentConfig {
        disabled: true,
        provider: None,
    });
    println!(
        "subagent model disabled; run `acps subagent set` to configure, or `acps subagent match` to inherit later"
    );
    Ok(true)
}

pub(super) fn apply_supabase_env_defaults(args: &mut InitArgs) -> Result<()> {
    let explicit_supabase_args = args.supabase_url.is_some()
        || args.supabase_schema.is_some()
        || args.supabase_api_key_ref.is_some();

    if args.no_supabase {
        return Ok(());
    }

    let enabled = match env_value(SUPABASE_ENABLED_ENV) {
        Some(value) => Some(parse_supabase_enabled_env(&value)?),
        None => None,
    };

    if enabled == Some(false) && !explicit_supabase_args {
        args.no_supabase = true;
        return Ok(());
    }

    if args.supabase_url.is_none() {
        args.supabase_url = env_value(SUPABASE_URL_ENV);
    }
    if args.supabase_schema.is_none() {
        args.supabase_schema = env_value(SUPABASE_SCHEMA_ENV);
    }
    if args.supabase_api_key_ref.is_none() {
        args.supabase_api_key_ref = env_value(SUPABASE_API_KEY_REF_ENV);
    }

    if enabled == Some(true) && args.supabase_url.is_none() {
        return Err(StackError::MissingField {
            field: SUPABASE_URL_ENV,
        });
    }

    if args.supabase_url.is_none()
        && (args.supabase_schema.is_some() || args.supabase_api_key_ref.is_some())
    {
        return Err(StackError::InvalidParam {
            field: "--supabase-url",
            reason: "required when setting Supabase schema or API-key ref during init".to_owned(),
        });
    }

    Ok(())
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn parse_supabase_enabled_env(value: &str) -> Result<bool> {
    match value {
        "1" | "true" | "TRUE" | "yes" | "YES" => Ok(true),
        "0" | "false" | "FALSE" | "no" | "NO" => Ok(false),
        _ => Err(StackError::InvalidParam {
            field: SUPABASE_ENABLED_ENV,
            reason: "must be 0, 1, true, false, yes, or no".to_owned(),
        }),
    }
}

pub(super) fn apply_supabase_to_config_for_init(
    args: &InitArgs,
    config: &mut Config,
) -> Result<bool> {
    if args.no_supabase {
        let mut supabase = config
            .logging
            .supabase
            .clone()
            .unwrap_or_else(disabled_supabase_config);
        supabase.enabled = false;
        return apply_supabase_config(config, supabase);
    }

    let Some(url) = args.supabase_url.clone() else {
        return Ok(false);
    };
    apply_supabase_config(
        config,
        enabled_supabase_config(
            url,
            Some(
                args.supabase_schema
                    .clone()
                    .unwrap_or_else(|| SUPABASE_DEFAULT_SCHEMA.to_owned()),
            ),
            Some(
                args.supabase_api_key_ref
                    .clone()
                    .unwrap_or_else(|| SUPABASE_DEFAULT_API_KEY_REF.to_owned()),
            ),
        ),
    )
}

pub(super) fn reject_supabase_init_args_for_existing_config(args: &InitArgs) -> Result<()> {
    if args.supabase_url.is_some()
        || args.supabase_schema.is_some()
        || args.supabase_api_key_ref.is_some()
        || args.no_supabase
    {
        return Err(StackError::InvalidParam {
            field: "--supabase-url",
            reason: "Supabase init setup applies only when creating a starter config; use `acps logging supabase` for initialized instances".to_owned(),
        });
    }
    Ok(())
}
