use super::*;

/// Step: secrets_init — generate or preserve session + admin verifiers, then
/// settle every secret the config declares (operator agent env, MCP and data
/// source refs, Supabase) and validate the staged native config against them.
/// Verifier: both verifier rows present in state.
pub(super) fn run_secrets_phase(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    init_println!(output_mode, "progress: initializing auth");
    // Hosted rotation was already folded into the flag at entry, so this
    // reads the single source of truth (and any replayed recorded value).
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
    let secret_store = &mut flow.secret_store;
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
            let outcome = perform_auth_init(store, legacy_auth, home, secret_store, key_policy)?;
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
    let disposition = match step_result {
        Ok(d) => d,
        Err(error) => return finalize_with_error(&flow.store, &flow.init_run, error),
    };
    // Honest "auth:" line for the skipped path — we did not generate keys
    // this run, we trusted the verifier instead.
    if matches!(disposition, StepDisposition::Skipped) {
        flow.key_handover.auth_ready = true;
        flow.auth_status = "preserved existing API keys";
    }
    // Write interactively-collected agent env values and verify flag-provided
    // refs now that the store is open, before the agent is installed/launched so
    // `resolve_agent_env` resolves them. The ref names are appended to
    // `agent.env` only AFTER verification succeeds, so a run that fails here never
    // persists an unresolved ref (which a later `--resume` would otherwise
    // complete around). No-op when nothing was collected (a resume or an existing
    // config).
    let env_apply = (|| -> Result<()> {
        apply_agent_env_collection(&mut flow.secret_store, &flow.agent_env_collection)?;
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
    // An imported or hand-edited kilo config may not declare KILO_API_KEY;
    // without the declaration the variable never reaches the agent's process
    // env, which the harness rejects even when a provider-native credential
    // carries the auth. Seed the declaration like `agent set --model` does,
    // then let the placeholder below fill its value.
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
    // A kilo config authenticating through a provider-native credential —
    // imported, or declared via `--agent-env-ref` and just applied above —
    // still needs KILO_API_KEY present in the agent's process env, so record
    // the empty placeholder during init rather than making the operator run
    // a separate `secrets set` afterwards. Uses the phase's own store handle:
    // the store is a whole-file read-modify-write, so a second handle's later
    // writes would clobber the placeholder.
    match crate::cli::agent::record_empty_key_placeholders_for_provider_native_env(
        &mut flow.secret_store,
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
    // Offer masked entry for secret refs declared by MCP servers and S3 data
    // sources (flags, wizard, or hosted request). Skipped refs are not an
    // error: they surface later in MCP health or workspace materialization,
    // and a hosted backend may push them through the secrets API post-init.
    // Resume runs re-offer refs that were skipped on the failed attempt.
    if flow.creating_config || flow.args.resume {
        match collect_declared_secret_refs_for_init(
            prompts_enabled(&flow.args),
            &flow.config,
            &mut flow.secret_store,
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
            &mut flow.secret_store,
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
            &mut flow.secret_store,
        ) {
            return finalize_with_error(&flow.store, &flow.init_run, error);
        }
        // A hosted init may have just soft-passed a custom provider ref that is
        // pending a managed credential push; resolving the prepared environment
        // would hard-fail on that same ref, so only the unrelated MCP refs are
        // validated until the credential lands.
        let validation = match pending_custom_provider_credential(
            &prepared.canonical_config,
            &flow.secret_store,
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
