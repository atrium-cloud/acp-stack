use crate::api::{self, AppState, RuntimePaths};
use crate::auth::{AuthVerifierEnsureOutcome, ensure_auth_verifier_pair};
use crate::config::{self, SupabaseLoggingBackend};
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_dir,
    set_owner_only_file,
};
use crate::runtime::agent::stale_prompt_sweeper::StalePromptSweeper;
use crate::runtime::agent::supervisor::ServerLifecycle;
use crate::runtime::install::agent_auto_update::AgentAutoUpdater;
use crate::runtime::logging::supabase_mirror::SUPABASE_DEFAULT_DB_URL_REF;
use crate::runtime::logging::supabase_sink::{SupabaseSink, SupabaseSinkCredential};
use crate::secrets::SecretStore;
use crate::state::{StateStore, default_state_path};
use clap::Args;

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Override the `api.bind` address from config.
    #[arg(long)]
    bind: Option<String>,
    /// Development-only opt-in to running the daemon as root. Even with this
    /// flag set, the admin API key must be non-empty.
    #[cfg(feature = "dev-tools")]
    #[arg(long, hide = true)]
    allow_root: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServeMode {
    Operator,
    Dev,
}

const ALLOW_ROOT_ENV: &str = "ACP_STACK_ALLOW_ROOT";

/// Cap on the command-id list embedded in the `server.reconciled` event payload.
const RECONCILED_COMMAND_IDS_CAP: usize = 50;

fn allow_root_env_enabled() -> bool {
    std::env::var(ALLOW_ROOT_ENV).is_ok_and(|value| value == "1")
}

impl ServeArgs {
    fn allow_root(&self) -> bool {
        #[cfg(feature = "dev-tools")]
        {
            self.allow_root
        }
        #[cfg(not(feature = "dev-tools"))]
        {
            false
        }
    }
}

/// Refuse to serve as root unless explicitly opted in.
fn check_root_constraints(euid: u32, allow_root: bool) -> Result<()> {
    if euid != 0 {
        return Ok(());
    }
    if !allow_root {
        return Err(StackError::ServeRefusedAsRoot);
    }
    Ok(())
}

pub(super) fn run_serve(args: ServeArgs, mode: ServeMode) -> Result<()> {
    run_serve_with_euid(args, mode, crate::ownership::process_euid())
}

fn run_serve_with_euid(args: ServeArgs, mode: ServeMode, process_euid: u32) -> Result<()> {
    if args.allow_root() && mode != ServeMode::Dev {
        return Err(StackError::InvalidParam {
            field: "--allow-root",
            reason: "development-only flag; use `acps dev serve --allow-root`".to_owned(),
        });
    }
    if allow_root_env_enabled() && mode != ServeMode::Dev {
        return Err(StackError::InvalidParam {
            field: ALLOW_ROOT_ENV,
            reason: "development-only environment override; use `acps dev serve`".to_owned(),
        });
    }
    let allow_root = mode == ServeMode::Dev && (args.allow_root() || allow_root_env_enabled());
    if process_euid == 0 && !allow_root {
        return Err(StackError::ServeRefusedAsRoot);
    }

    let home = home_dir()?;
    let config_path = config::default_config_path()?;
    let config_dir = parent_dir(&config_path)?;
    if config_dir.exists() {
        set_owner_only_dir(config_dir)?;
    }
    if config_path.exists() {
        set_owner_only_file(&config_path)?;
    }
    let loaded_config = config::load_for_serve(&config_path)?;
    let config = loaded_config.config;

    // Fail closed: a configured sandbox backend that cannot run on this host must refuse to serve
    // rather than silently lose the security posture at the first agent spawn.
    if config.workspace.sandbox.mode != config::SandboxMode::Off
        && let Err(reason) = crate::runtime::sandbox::preflight(
            &config.workspace.sandbox,
            crate::extensions::resolve_network_provider(&config).as_ref(),
        )
    {
        return Err(crate::error::StackError::SandboxFailed { reason });
    }

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let mut store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;
    match ensure_auth_verifier_pair(&store, loaded_config.legacy_auth.as_ref(), &home)? {
        AuthVerifierEnsureOutcome::Preserved
        | AuthVerifierEnsureOutcome::BackfilledLegacySecrets => {}
        AuthVerifierEnsureOutcome::Missing => {
            return Err(StackError::MissingField {
                field: "auth_keys.session and auth_keys.admin",
            });
        }
    }
    let auth_verifiers = store.load_auth_verifier_pair()?;

    // Ordering: the secret store and Supabase settings must resolve BEFORE the startup reconciles,
    // or their terminal-status writes land before the outbox flag flips and never reach Supabase.
    let secret_store = SecretStore::open(&home)?;

    check_root_constraints(process_euid, allow_root)?;
    if process_euid == 0 {
        tracing::warn!("acps dev serve running as root with explicit development opt-in");
    }

    // Per spec, a disabled stanza must never reach into the secret store.
    let supabase_settings = if config.logging.supabase.as_ref().is_some_and(|s| s.enabled) {
        let supabase = config
            .logging
            .supabase
            .as_ref()
            .expect("checked is_some_and above");
        let credential = match supabase.backend {
            SupabaseLoggingBackend::Postgrest => {
                if !secret_store.contains(&supabase.api_key_ref) {
                    return Err(StackError::MissingSupabaseApiKey {
                        name: supabase.api_key_ref.clone(),
                    });
                }
                SupabaseSinkCredential::PostgrestApiKey(
                    secret_store.get(&supabase.api_key_ref)?.to_owned(),
                )
            }
            SupabaseLoggingBackend::Postgres => {
                let Some(db_url_ref) = supabase.db_url_ref.as_ref() else {
                    return Err(StackError::MissingSupabaseDbUrl {
                        name: SUPABASE_DEFAULT_DB_URL_REF.to_owned(),
                    });
                };
                if !secret_store.contains(db_url_ref) {
                    return Err(StackError::MissingSupabaseDbUrl {
                        name: db_url_ref.clone(),
                    });
                }
                SupabaseSinkCredential::PostgresDbUrl(secret_store.get(db_url_ref)?.to_owned())
            }
        };
        store.set_external_logging_enabled(true);
        Some((supabase.clone(), credential))
    } else {
        None
    };

    // Startup reconcile sweeps, each independently best-effort so a transient failure leaves the
    // missed rows for the next restart. Permissions sweep BEFORE commands so a partial run can only
    // strand a command in `pending`, never fail a command while leaving its permission approvable.
    let reconciled_prompts = match store.reconcile_orphaned_prompts("daemon restart") {
        Ok(reconciled) => {
            if reconciled > 0 {
                tracing::info!(
                    reconciled,
                    "marked orphaned in-flight prompts as errored on startup"
                );
            }
            reconciled
        }
        Err(error) => {
            tracing::warn!(error = %error, "startup prompt reconcile failed; rows will settle on the next restart");
            0
        }
    };

    // ACP-source permission rows are canceled (their request channel is gone); command-source rows
    // are expired instead.
    let (perm_canceled, perm_expired) = match store.reconcile_orphaned_permissions() {
        Ok((canceled, expired)) => {
            if canceled > 0 || expired > 0 {
                tracing::info!(
                    canceled,
                    expired,
                    "settled orphaned permission requests on startup"
                );
            }
            (canceled, expired)
        }
        Err(error) => {
            tracing::warn!(error = %error, "startup permission reconcile failed; rows will settle on the next restart");
            (0, 0)
        }
    };

    // `kill_on_drop` reaps mediated subprocesses on restart without finalizing their `commands` rows.
    let (reconciled_command_ids, command_permissions_canceled) = match store
        .reconcile_orphaned_commands()
    {
        Ok((ids, permissions_canceled)) => {
            if !ids.is_empty() {
                tracing::info!(
                    reconciled = ids.len(),
                    permissions_canceled,
                    "marked orphaned in-flight commands as failed on startup"
                );
            }
            (ids, permissions_canceled)
        }
        Err(error) => {
            tracing::warn!(error = %error, "startup command reconcile failed; rows will settle on the next restart");
            (Vec::new(), 0)
        }
    };

    let bind = args.bind.unwrap_or_else(|| config.api.bind.clone());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;

    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .map_err(|source| StackError::ServeBind {
                bind: bind.clone(),
                source,
            })?;
        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| bind.clone());
        let (socket_path, parent_policy) = match config.local.socket_path.as_deref() {
            Some(path) => (
                std::path::PathBuf::from(path),
                crate::local_listener::ParentPolicy::ValidateOwnerOnly,
            ),
            None => (
                crate::local_listener::default_socket_path()?,
                crate::local_listener::ParentPolicy::RepairOwnerOnly,
            ),
        };
        // `server.starting` is recorded only after every listener is bound, so a bind failure
        // cannot leave a dangling lifecycle row.
        let bound_local = crate::local_listener::bind_local(&socket_path, parent_policy).await?;
        let lifecycle = ServerLifecycle::starting(&store, &local)?;
        let runtime_paths = RuntimePaths::new(config_path, state_path);
        let app_state = AppState::with_auth_verifiers_and_runtime_paths(
            config,
            store,
            auth_verifiers,
            local.clone(),
            runtime_paths,
        );
        crate::api::routes::native_config::recover_native_config_imports(&app_state).await?;
        let state_handle = app_state.state.clone();
        let event_hub = app_state.event_hub.clone();
        lifecycle.started(&state_handle, &event_hub, &local).await?;

        // Emitted after the hub is attached to the store so the sweep trace also fans out live.
        if reconciled_prompts > 0
            || perm_canceled > 0
            || perm_expired > 0
            || !reconciled_command_ids.is_empty()
        {
            let command_ids_truncated = reconciled_command_ids.len() > RECONCILED_COMMAND_IDS_CAP;
            let command_ids: Vec<&String> = reconciled_command_ids
                .iter()
                .take(RECONCILED_COMMAND_IDS_CAP)
                .collect();
            let payload = serde_json::json!({
                "prompts": reconciled_prompts,
                "commands": reconciled_command_ids.len(),
                "permissions_cancelled": perm_canceled + command_permissions_canceled,
                "permissions_expired": perm_expired,
                "command_ids": command_ids,
                "command_ids_truncated": command_ids_truncated,
                "reason": "daemon-restart",
            });
            let appended = {
                let store = state_handle.lock().await;
                store.append_event(
                    "info",
                    "server.reconciled",
                    "settled orphaned rows on startup",
                    &payload.to_string(),
                )
            };
            if let Err(error) = appended {
                tracing::warn!(error = %error, "failed to record server.reconciled event");
            }
        }
        eprintln!("acps serve: listening on {local}");
        eprintln!("acps serve: local socket at {}", socket_path.display());
        let agent_supervisor = app_state.agent_supervisor.clone();
        let agent_targets = app_state.agent_targets.clone();

        // Sink construction failures are fatal at boot: no fallback preserves at-least-once delivery.
        let supabase_sink = match supabase_settings {
            Some((supabase, key)) => Some(SupabaseSink::spawn(
                state_handle.clone(),
                supabase,
                key,
                event_hub.clone(),
            )?),
            None => None,
        };

        // Held in scope so the sweeper shuts down before `acps serve` returns.
        let stale_prompt_sweeper = StalePromptSweeper::spawn(
            state_handle.clone(),
            app_state.config.prompts.effective_stale_threshold(),
            app_state.config.prompts.effective_sweep_interval(),
        );
        let agent_auto_updater = AgentAutoUpdater::spawn(
            home.clone(),
            app_state.runtime_paths.as_ref().config_path.clone(),
            app_state.runtime_paths.as_ref().state_path.clone(),
            state_handle.clone(),
            agent_supervisor.clone(),
        );

        // Both listeners share one graceful-shutdown signal; aborting the local task lets its
        // `SocketGuard::drop` unlink the socket when TCP serve exits first.
        let local_handle = tokio::spawn(crate::local_listener::serve_local(
            app_state.clone(),
            bound_local,
        ));
        let serve_result = api::serve(app_state, listener).await;
        local_handle.abort();
        match local_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(error = %err, "local listener exited with error"),
            Err(join_err) if join_err.is_cancelled() => {}
            Err(join_err) => {
                tracing::warn!(error = %join_err, "local listener task panicked")
            }
        }
        // Tear down agents BEFORE recording `server.stopped`, and keep the Supabase sink alive
        // across both so `agent.stopped` and `server.stopped` still reach the external mirror.
        for target in agent_targets.targets() {
            target
                .supervisor
                .shutdown_on_serve_exit(&target.target_id, &state_handle, &event_hub)
                .await;
        }
        // Stop the sweeper before `server.stopped`, or a racing sweep appends `prompt.stalled`
        // after the lifecycle row.
        stale_prompt_sweeper.shutdown().await;
        agent_auto_updater.shutdown().await;
        let reason = match &serve_result {
            Ok(()) => "signal",
            Err(_) => "error",
        };
        // A failing lifecycle write must not mask the original serve error.
        if let Err(err) = lifecycle.stopped(&state_handle, &event_hub, reason).await {
            tracing::error!(error = %err, "failed to record server.stopped");
        }

        // Drain AFTER `agent.stopped` and `server.stopped` land in the outbox.
        if let Some(sink) = supabase_sink {
            sink.shutdown().await;
        }

        eprintln!("acps serve: stopped ({reason})");
        serve_result
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ALLOW_ROOT_ENV, ServeArgs, ServeMode, StackError, allow_root_env_enabled,
        check_root_constraints, run_serve_with_euid,
    };
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn non_root_euid_passes_unconditionally() {
        check_root_constraints(1000, false).expect("non-root euid bypasses the gate");
    }

    #[test]
    fn root_without_opt_in_is_refused() {
        let err = check_root_constraints(0, false).expect_err("root must be refused");
        assert!(matches!(err, StackError::ServeRefusedAsRoot));
    }

    #[test]
    fn root_with_opt_in_and_admin_key_is_allowed() {
        check_root_constraints(0, true).expect("root + opt-in is allowed");
    }

    #[test]
    fn allow_root_env_requires_exact_one() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var(ALLOW_ROOT_ENV);
        }
        assert!(!allow_root_env_enabled());

        unsafe {
            std::env::set_var(ALLOW_ROOT_ENV, "");
        }
        assert!(!allow_root_env_enabled());

        unsafe {
            std::env::set_var(ALLOW_ROOT_ENV, "0");
        }
        assert!(!allow_root_env_enabled());

        unsafe {
            std::env::set_var(ALLOW_ROOT_ENV, "1");
        }
        assert!(allow_root_env_enabled());

        unsafe {
            std::env::remove_var(ALLOW_ROOT_ENV);
        }
    }

    #[test]
    fn root_without_opt_in_refuses_before_state_creation() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let tempdir = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var(ALLOW_ROOT_ENV);
            std::env::set_var("HOME", tempdir.path());
        }

        let err = run_serve_with_euid(
            ServeArgs {
                bind: None,
                #[cfg(feature = "dev-tools")]
                allow_root: false,
            },
            ServeMode::Operator,
            0,
        )
        .expect_err("root without opt-in must fail before reading config or state");
        assert!(matches!(err, StackError::ServeRefusedAsRoot));
        assert!(
            !tempdir.path().join(".local").exists(),
            "root refusal must not create state directories"
        );

        unsafe {
            if let Some(home) = previous_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}
