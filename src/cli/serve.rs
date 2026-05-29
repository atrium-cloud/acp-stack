use crate::api::{self, AppState, RuntimePaths};
use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_dir,
    set_owner_only_file,
};
use crate::runtime::agent::stale_prompt_sweeper::StalePromptSweeper;
use crate::runtime::agent::supervisor::ServerLifecycle;
use crate::runtime::logging::supabase_sink::SupabaseSink;
use crate::secrets::SecretStore;
use crate::state::{StateStore, default_state_path};
use clap::Args;

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Override the `api.bind` address from config.
    #[arg(long)]
    bind: Option<String>,
    /// Opt-in to running the daemon as root. Equivalent to setting
    /// `ACP_STACK_ALLOW_ROOT=1`. Intended only for disposable/dev profiles
    /// (e.g. ephemeral containers) — production deployments run as the
    /// configured `workspace.runtime_user`. Even with this flag set, the
    /// admin API key must be non-empty.
    #[arg(long)]
    allow_root: bool,
}

const ALLOW_ROOT_ENV: &str = "ACP_STACK_ALLOW_ROOT";

fn allow_root_env_enabled() -> bool {
    std::env::var(ALLOW_ROOT_ENV).is_ok_and(|value| value == "1")
}

/// Refuse to serve as root unless explicitly opted in, and never allow the
/// daemon to run as root with an unset admin API key — the admin key gates
/// every mutating route, and an empty admin key combined with root execution
/// is an open back door.
fn check_root_constraints(euid: u32, allow_root: bool, admin_key_empty: bool) -> Result<()> {
    if euid != 0 {
        return Ok(());
    }
    if !allow_root {
        return Err(StackError::ServeRefusedAsRoot);
    }
    if admin_key_empty {
        return Err(StackError::ServeRootRequiresAdminKey);
    }
    Ok(())
}

pub(super) fn run_serve(args: ServeArgs) -> Result<()> {
    run_serve_with_euid(args, crate::ownership::process_euid())
}

fn run_serve_with_euid(args: ServeArgs, process_euid: u32) -> Result<()> {
    let allow_root = args.allow_root || allow_root_env_enabled();
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
    let config = Config::load_from_path(&config_path)?;

    let state_path = default_state_path(&home);
    let state_dir = parent_dir(&state_path)?;
    create_dir_owner_only(state_dir)?;
    pre_create_owner_only(&state_path)?;
    let mut store = StateStore::open(&state_path)?;
    store.migrate()?;
    set_owner_only_file(&state_path)?;

    // Open the secret store + resolve any Supabase settings BEFORE running
    // startup reconciles. The reconciles transition orphaned prompt/command/
    // permission rows to terminal status; if external logging is enabled but
    // the flag isn't flipped yet, those terminal writes don't enqueue into
    // the outbox and Supabase would never see the post-crash settlement.
    let secret_store = SecretStore::open(&home)?;
    let session_ref = config.auth.session_key_ref.clone();
    let admin_ref = config.auth.admin_key_ref.clone();
    if !secret_store.contains(&session_ref) {
        return Err(StackError::MissingSessionKey { name: session_ref });
    }
    if !secret_store.contains(&admin_ref) {
        return Err(StackError::MissingAdminKey { name: admin_ref });
    }
    let session_key = secret_store.get(&session_ref)?.to_owned();
    let admin_key = secret_store.get(&admin_ref)?.to_owned();

    // Gate root execution behind an explicit opt-in. The daemon never drops
    // privileges itself, so a `User=` directive in the systemd unit or a
    // non-root `USER` in the Dockerfile is the production path. The CLI flag
    // / env var exists only for disposable/dev profiles, and even then an
    // empty admin key is refused — see Phase 4 line 47 of the todo list.
    check_root_constraints(process_euid, allow_root, admin_key.is_empty())?;
    if process_euid == 0 {
        tracing::warn!(
            "acps serve running as root with explicit opt-in; intended for disposable/dev profiles only"
        );
    }

    // Resolve the Supabase secret API key only when external logging is
    // explicitly enabled; per spec, a disabled stanza must never reach into
    // the secret store. The outbox flag has to flip BEFORE the startup
    // reconciles so their terminal-status writes get mirrored.
    let supabase_settings = if config.logging.supabase.as_ref().is_some_and(|s| s.enabled) {
        let supabase = config
            .logging
            .supabase
            .as_ref()
            .expect("checked is_some_and above");
        if !secret_store.contains(&supabase.api_key_ref) {
            return Err(StackError::MissingSupabaseApiKey {
                name: supabase.api_key_ref.clone(),
            });
        }
        let key = secret_store.get(&supabase.api_key_ref)?.to_owned();
        store.set_external_logging_enabled(true);
        Some((supabase.clone(), key))
    } else {
        None
    };

    // Reconcile any prompts left in-flight by a previous crash/restart.
    // The in-memory task registry is empty at startup, so a `pending` or
    // `running` row from before would never get a terminal status without
    // this sweep, leaving CLI/HTTP clients polling forever.
    let reconciled = store.reconcile_orphaned_prompts("daemon restart")?;
    if reconciled > 0 {
        tracing::info!(
            reconciled,
            "marked orphaned in-flight prompts as errored on startup"
        );
    }

    // Same sweep for the command gateway: a daemon restart kills any
    // mediated subprocesses via `kill_on_drop`, but their `commands` rows
    // are not finalized along the way. Mark them `failed` so polling
    // clients see them settle.
    let reconciled_commands = store.reconcile_orphaned_commands("daemon restart")?;
    if reconciled_commands > 0 {
        tracing::info!(
            reconciled = reconciled_commands,
            "marked orphaned in-flight commands as failed on startup"
        );
    }

    // Reconcile pending permission rows. ACP-source rows are canceled (the
    // request channel is gone after restart); command-source rows are
    // expired so the spec's "clients never see them stay pending forever"
    // promise holds across restart.
    let (perm_canceled, perm_expired) = store.reconcile_orphaned_permissions()?;
    if perm_canceled > 0 || perm_expired > 0 {
        tracing::info!(
            canceled = perm_canceled,
            expired = perm_expired,
            "settled orphaned permission requests on startup"
        );
    }

    let bind = args.bind.unwrap_or_else(|| config.api.bind.clone());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;

    runtime.block_on(async move {
        // Bind first, then record `server.starting`. Recording before bind
        // would leave a dangling start row whenever the address is already
        // in use; pairing the lifecycle write with a successful bind keeps
        // the durable trail truthful.
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
        let (socket_path, parent_policy) = match config.acpctl.socket_path.as_deref() {
            Some(path) => (
                std::path::PathBuf::from(path),
                crate::local_listener::ParentPolicy::ValidateOwnerOnly,
            ),
            None => (
                crate::local_listener::default_socket_path()?,
                crate::local_listener::ParentPolicy::RepairOwnerOnly,
            ),
        };
        // Bind the acpctl UDS *and* record `server.starting` only after every
        // listener is ready. A bind failure here is a startup-time error, not
        // a post-start regression — emitting `server.starting` first would
        // leave a dangling lifecycle row whenever the UDS bind fails.
        let bound_local = crate::local_listener::bind_local(&socket_path, parent_policy).await?;
        let lifecycle = ServerLifecycle::starting(&store, &local)?;
        let runtime_paths = RuntimePaths::new(config_path, state_path);
        let app_state = AppState::with_effective_bind_and_runtime_paths(
            config,
            store,
            session_key,
            admin_key,
            local.clone(),
            runtime_paths,
        );
        let state_handle = app_state.state.clone();
        let event_hub = app_state.event_hub.clone();
        lifecycle.started(&state_handle, &event_hub, &local).await?;
        eprintln!("acps serve: listening on {local}");
        eprintln!("acps serve: acpctl socket at {}", socket_path.display());
        let agent_supervisor = app_state.agent_supervisor.clone();

        // Spawn the Supabase sink once the runtime + shared state are ready.
        // Failures to build the HTTP client are fatal at boot — there is no
        // good fallback that preserves the spec's at-least-once delivery
        // guarantee, and we'd rather not start the server than silently lose
        // outbound events.
        let supabase_sink = match supabase_settings {
            Some((supabase, key)) => Some(SupabaseSink::spawn(
                state_handle.clone(),
                supabase,
                key,
                event_hub.clone(),
            )?),
            None => None,
        };

        // Background sweep for prompts whose agent stopped streaming. Without
        // this task an agent that hangs mid-stream leaves the `prompts` row
        // in `running` forever; the sweeper flips it to terminal `Stalled`
        // so polling clients always see settlement. Held in scope so it
        // shuts down before `acps serve` returns.
        let stale_prompt_sweeper = StalePromptSweeper::spawn(
            state_handle.clone(),
            app_state.config.prompts.effective_stale_threshold(),
            app_state.config.prompts.effective_sweep_interval(),
        );

        // The acpctl UDS server runs alongside the TCP server. Both subscribe
        // to the same SIGTERM/SIGINT handler via `axum::serve.with_graceful_shutdown`,
        // so a single signal stops both. If the TCP serve exits first, the
        // local task is aborted; its `SocketGuard::drop` unlinks the socket.
        let local_handle = tokio::spawn(crate::local_listener::serve_local(
            app_state.clone(),
            bound_local,
        ));
        let serve_result = api::serve(app_state, listener).await;
        local_handle.abort();
        match local_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(error = %err, "acpctl local listener exited with error"),
            Err(join_err) if join_err.is_cancelled() => {}
            Err(join_err) => {
                tracing::warn!(error = %join_err, "acpctl local listener task panicked")
            }
        }
        // Tear down the agent BEFORE recording server.stopped so the
        // durable trail shows the agent went down first. A leaked agent
        // process outliving the daemon is a real bug, not a theoretical
        // one: see superflous-restart guarantees in security.md.
        //
        // The Supabase sink is intentionally kept alive across both this
        // shutdown step and `lifecycle.stopped` below: both write durable
        // rows (`agent.stopped`, `server.stopped`) that must reach the
        // external mirror. The sink's own shutdown does one final drain
        // pass before exiting so those rows are uploaded.
        agent_supervisor
            .shutdown_on_serve_exit(&state_handle, &event_hub)
            .await;
        // Stop the stale-prompt sweeper before recording `server.stopped`.
        // Otherwise a sweep racing with shutdown could append a
        // `prompt.stalled` event after the lifecycle row, muddling the
        // durable shutdown trail.
        stale_prompt_sweeper.shutdown().await;
        let reason = match &serve_result {
            Ok(()) => "signal",
            Err(_) => "error",
        };
        // Always record stopped, even on error. Failures from the second
        // lifecycle write are logged but do not mask the original serve error.
        if let Err(err) = lifecycle.stopped(&state_handle, &event_hub, reason).await {
            tracing::error!(error = %err, "failed to record server.stopped");
        }

        // Drain the Supabase sink AFTER agent.stopped + server.stopped have
        // landed in the outbox so the final lifecycle rows reach Supabase.
        // The sink's shutdown runs one last batch loop before exiting; any
        // individual failure persists in `sink_failures_summary` and gets
        // surfaced on the next start via `security check`.
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
        ALLOW_ROOT_ENV, ServeArgs, StackError, allow_root_env_enabled, check_root_constraints,
        run_serve_with_euid,
    };
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn non_root_euid_passes_unconditionally() {
        check_root_constraints(1000, false, false).expect("non-root euid bypasses the gate");
        check_root_constraints(1000, false, true).expect("non-root euid bypasses the gate");
    }

    #[test]
    fn root_without_opt_in_is_refused() {
        let err = check_root_constraints(0, false, false).expect_err("root must be refused");
        assert!(matches!(err, StackError::ServeRefusedAsRoot));
    }

    #[test]
    fn root_with_opt_in_but_empty_admin_key_is_refused() {
        let err = check_root_constraints(0, true, true)
            .expect_err("root + empty admin key must be refused");
        assert!(matches!(err, StackError::ServeRootRequiresAdminKey));
    }

    #[test]
    fn root_with_opt_in_and_admin_key_is_allowed() {
        check_root_constraints(0, true, false).expect("root + admin key + opt-in is allowed");
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
                allow_root: false,
            },
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
