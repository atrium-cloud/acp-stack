use super::*;

/// Step: agent_headless_config — write the agent's local config files so the
/// harness can start without first-run prompts.
pub(super) fn run_agent_headless_config_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    init_println!(output_mode, "progress: writing agent headless config");
    let home = &flow.home;
    let config = &flow.config;
    let provisioned_agent_configs = &mut flow.provisioned_agent_configs;
    let result = record_init_step(
        &flow.store,
        &flow.init_run,
        5,
        step_kind::AGENT_HEADLESS_CONFIG,
        || {
            // Provision is idempotent and cheap, so the verifier always says
            // no: a resume must re-derive output from a possibly-changed
            // config.
            Ok(false)
        },
        || {
            let candidate_paths = headless_config_candidate_paths(&config.agent.id, home);
            let snapshots = capture_path_snapshots(&candidate_paths)?;
            let mut dir_scan = candidate_paths
                .iter()
                .filter_map(|path| path.parent().map(Path::to_path_buf))
                .collect::<Vec<_>>();
            dir_scan.extend(headless_config_side_dirs(&config.agent.id, home));
            let dir_listings = capture_dir_listings_for(&dir_scan)?;

            crate::runtime::agent::provider_model_catalog::refresh_provider_models_best_effort_blocking(
                home, config,
            );
            match crate::runtime::agent::agent_headless_config::provision_agent_headless_config(
                config, home,
            ) {
                Ok(paths) => {
                    *provisioned_agent_configs = paths;
                    Ok(StepOutcome::empty())
                }
                Err(error) => {
                    restore_headless_snapshots(snapshots);
                    remove_new_files_in_dirs(dir_listings);
                    Err(error)
                }
            }
        },
    );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}

/// Step: edge_artifacts — render Cloudflare config files when an edge profile
/// was requested.
pub(super) fn run_edge_artifacts_step(flow: &mut InitFlow) -> Result<()> {
    let output_mode = flow.output_mode;
    if !(flow.edge_requested
        || step_needs_resume(&flow.prior_init_steps, step_kind::EDGE_ARTIFACTS))
    {
        return Ok(());
    }
    init_println!(output_mode, "progress: preparing Cloudflare edge artifacts");
    let config_path = &flow.config_path;
    let secret_store = flow.secret_store.clone();
    let config = &mut flow.config;
    let provisioned_edge_artifacts = &mut flow.provisioned_edge_artifacts;
    let result =
        record_init_step(
            &flow.store,
            &flow.init_run,
            6,
            step_kind::EDGE_ARTIFACTS,
            || Ok(false),
            || {
                let config_dir = parent_dir(config_path)?;
                *provisioned_edge_artifacts =
                    match config.edge.cloudflare.as_ref() {
                        Some(cloudflare) if cloudflare.enabled && cloudflare.mode == "managed" => {
                            let service_url = crate::edge::service_url_from_bind(&config.api.bind)?;
                            let api_token_ref = cloudflare.api_token_ref.clone().ok_or(
                                StackError::MissingField {
                                    field: "edge.cloudflare.api_token_ref",
                                },
                            )?;
                            let account_id_ref = cloudflare.account_id_ref.clone().ok_or(
                                StackError::MissingField {
                                    field: "edge.cloudflare.account_id_ref",
                                },
                            )?;
                            let api_token = lock_shared_secret_store(&secret_store)
                                .get(&api_token_ref)?
                                .to_owned();
                            let account_id = lock_shared_secret_store(&secret_store)
                                .get(&account_id_ref)?
                                .to_owned();
                            let created_tunnel = {
                                let cloudflare = config.edge.cloudflare.as_mut().ok_or(
                                    StackError::MissingField {
                                        field: "edge.cloudflare",
                                    },
                                )?;
                                crate::edge::ensure_managed_cloudflare_tunnel(
                                    cloudflare,
                                    &api_token,
                                    &account_id,
                                )?
                            };
                            if created_tunnel {
                                let canonical = config.to_canonical_toml()?;
                                *config = config::load_config_from_str(&canonical)?;
                                atomic_write_owner_only(config_path, canonical.as_bytes())?;
                            }
                            let cloudflare = config.edge.cloudflare.as_ref().ok_or(
                                StackError::MissingField {
                                    field: "edge.cloudflare",
                                },
                            )?;
                            crate::edge::finish_managed_cloudflare_provisioning(
                                config_dir,
                                cloudflare,
                                &service_url,
                                &api_token,
                                &account_id,
                            )?
                        }
                        Some(cloudflare) if cloudflare.enabled => {
                            let service_url = crate::edge::service_url_from_bind(&config.api.bind)?;
                            crate::edge::write_cloudflare_artifacts(
                                config_dir,
                                cloudflare,
                                &service_url,
                            )?
                        }
                        _ => Vec::new(),
                    };
                Ok(StepOutcome::empty())
            },
        );
    if let Err(error) = result {
        return finalize_with_error(&flow.store, &flow.init_run, error);
    }
    Ok(())
}
