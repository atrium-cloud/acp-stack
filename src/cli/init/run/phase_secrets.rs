use super::*;

/// Step: secrets_init — generate or preserve the session + admin verifiers, settle
/// every secret the config declares, and validate the staged native config.
pub(super) fn run_secrets_phase(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    init_println!(output_mode, "progress: initializing auth");
    let rotate_keys = flow.args.rotate_keys;
    let key_policy = if rotate_keys {
        KeyPolicy::RotateExisting
    } else {
        KeyPolicy::PreserveExisting
    };
    let store = &flow.store;
    let home = &flow.home;
    let legacy_auth = flow.legacy_auth.as_ref();
    let auth_status = &mut flow.auth_status;
    let key_handover = &mut flow.key_handover;
    let mut secret_store = lock_shared_secret_store(&flow.secret_store);
    let step_result = record_init_step(
        store,
        &flow.init_run,
        1,
        step_kind::SECRETS_INIT,
        // A rotating run must never replay as Skipped: a skipped step emits
        // no plaintext, which is exactly the wedge rotation exists to fix.
        || {
            if rotate_keys {
                Ok(false)
            } else {
                store.auth_key_pair_present()
            }
        },
        || {
            let outcome =
                perform_auth_init(store, legacy_auth, home, &mut secret_store, key_policy)?;
            *auth_status = outcome.status;
            let generated_keys = outcome.generated_keys;
            let rotated = outcome.rotated_keys;
            key_handover.keys = outcome.fresh_keys;
            key_handover.auth_ready = true;
            if generated_keys {
                let (kind, message) = if rotated {
                    ("auth.keys_rotated", "rotated session and admin API keys")
                } else {
                    (
                        "auth.keys_generated",
                        "generated session and admin API keys",
                    )
                };
                store.append_event_with_source(
                    "info",
                    kind,
                    crate::state::EVENT_SOURCE_CLI,
                    message,
                    &serde_json::json!({
                        "key_kinds": ["session", "admin"],
                    })
                    .to_string(),
                )?;
            }
            Ok(StepOutcome::with_payload(
                serde_json::json!({
                    "key_kinds": ["session", "admin"],
                    "status": *auth_status,
                })
                .to_string(),
            ))
        },
    );
    // The guard covers only the tracked step body; every later consumer in this
    // phase re-locks so a deposit can land between them.
    drop(secret_store);
    let disposition = match step_result {
        Ok(d) => d,
        Err(error) => return finalize_with_error(&flow.store, &flow.init_run, error),
    };
    if matches!(disposition, StepDisposition::Skipped) {
        flow.key_handover.auth_ready = true;
        flow.auth_status = "preserved existing API keys";
    }
    // Ref names are appended to `agent.env` only AFTER verification succeeds, so a run
    // that fails here never persists an unresolved ref a later `--resume` skips over.
    let env_apply = (|| -> Result<()> {
        apply_agent_env_collection(
            &mut lock_shared_secret_store(&flow.secret_store),
            &flow.agent_env_collection,
        )?;
        if append_agent_env_refs(&mut flow.config, &flow.agent_env_collection) {
            let canonical = flow.config.to_canonical_toml()?;
            flow.config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&flow.config_path, canonical.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(error) = env_apply {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    // Kilo rejects a launch without KILO_API_KEY in its process env even when a
    // provider-native credential carries the auth, so seed the declaration here and
    // let the placeholder below fill its value.
    let kilo_env_seed = (|| -> Result<()> {
        if crate::cli::agent::seed_kilo_mapped_key_env_declaration(&mut flow.config.agent) {
            let canonical = flow.config.to_canonical_toml()?;
            flow.config = config::load_config_from_str(&canonical)?;
            atomic_write_owner_only(&flow.config_path, canonical.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(error) = kilo_env_seed {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    match crate::cli::agent::record_empty_key_placeholders_for_provider_native_env(
        &mut lock_shared_secret_store(&flow.secret_store),
        &flow.config.agent,
    ) {
        Ok(recorded) => {
            for placeholder in &recorded {
                init_println!(
                    output_mode,
                    "recorded empty {placeholder} placeholder: the harness requires the variable present; authentication uses the declared provider-native credential"
                );
            }
        }
        Err(error) => return finalize_with_error(&flow.store, &flow.init_run, error),
    }
    // Skipped refs are not an error: they surface later in MCP health or workspace
    // materialization, and a hosted backend may push them in post-init.
    if flow.creating_config || flow.args.resume {
        match collect_declared_secret_refs_for_init(
            prompts_enabled(&flow.args),
            &flow.config,
            &flow.secret_store,
        ) {
            Ok(stored) if !stored.is_empty() => {
                init_println!(output_mode, "declared secrets: set ({})", stored.join(", "));
            }
            Ok(_) => {}
            Err(error) => return finalize_with_error(&flow.store, &flow.init_run, error),
        }
    }
    if let Some(supabase) = flow.config.logging.supabase.as_ref()
        && supabase.enabled
    {
        let api_key_ref = supabase.api_key_ref.clone();
        let stored = match ensure_supabase_secret(
            &flow.secret_store,
            &api_key_ref,
            prompts_enabled(&flow.args),
        ) {
            Ok(stored) => stored,
            Err(error) => return finalize_with_error(&flow.store, &flow.init_run, error),
        };
        if stored {
            init_println!(output_mode, "supabase secret: set ({api_key_ref})");
        } else {
            init_println!(output_mode, "supabase secret: preserved ({api_key_ref})");
        }
    }

    if let Some(record) = flow.init_native_config_record.as_mut()
        && let Err(error) = native_config::rebase_for_init(
            record,
            &flow.config,
            &flow.config_path,
            &flow.state_path,
            &flow.home,
        )
    {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    if let Some(prepared) = flow
        .init_native_config_record
        .as_ref()
        .and_then(|record| record.prepared.as_ref())
    {
        if let Err(error) = collect_prepared_secret_refs_for_init(
            &flow.args,
            &flow.registry,
            &prepared.canonical_config,
            &flow.config_path,
            &flow.secret_store,
        ) {
            return finalize_with_error(&flow.store, &flow.init_run, error);
        }
        // Resolving the prepared environment would hard-fail on a provider ref still
        // pending a managed credential push, so only MCP refs are validated until it
        // lands.
        let validation = match pending_deferred_provider_credential(
            &prepared.canonical_config,
            &lock_shared_secret_store(&flow.secret_store),
        ) {
            Some((provider_id, api_key_ref)) => {
                init_println!(
                    output_mode,
                    "native config secret validation deferred: {}",
                    pending_provider_credential_reason(&provider_id, &api_key_ref)
                );
                crate::runtime::agent::native_config_import::validate_native_config_mcp_secret_refs(
                    prepared, &flow.home,
                )
            }
            None => {
                crate::runtime::agent::native_config_import::validate_native_config_secret_refs(
                    prepared, &flow.home,
                )
            }
        };
        if let Err(error) = validation {
            return finalize_with_error(&flow.store, &flow.init_run, error);
        }
    }
    Ok(())
}
