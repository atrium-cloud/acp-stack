use crate::api::{self, AppState, RuntimePaths};
use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::{
    create_dir_owner_only, home_dir, parent_dir, pre_create_owner_only, set_owner_only_dir,
    set_owner_only_file,
};
use crate::runtime::supabase_sink::SupabaseSink;
use crate::secrets::SecretStore;
use crate::state::{StateStore, default_state_path};
use crate::supervisor::ServerLifecycle;
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
#[cfg(debug_assertions)]
const FAKE_AGENT_TESTFLIGHT_MARKER: &str = ".acp-stack-testflight.txt";
#[cfg(debug_assertions)]
const FAKE_AGENT_TESTFLIGHT_CONTENT: &[u8] = b"acp-stack testflight ok\n";

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

    // Resolve the Supabase service-role secret only when external logging is
    // explicitly enabled; per spec, a disabled stanza must never reach into
    // the secret store. The outbox flag has to flip BEFORE the startup
    // reconciles so their terminal-status writes get mirrored.
    let supabase_settings = if config.logging.supabase.as_ref().is_some_and(|s| s.enabled) {
        let supabase = config
            .logging
            .supabase
            .as_ref()
            .expect("checked is_some_and above");
        if !secret_store.contains(&supabase.service_role_key_ref) {
            return Err(StackError::MissingSupabaseServiceRoleKey {
                name: supabase.service_role_key_ref.clone(),
            });
        }
        let key = secret_store.get(&supabase.service_role_key_ref)?.to_owned();
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

/// Minimal ACP agent fixture used only by integration tests. Reads NDJSON
/// JSON-RPC from stdin and writes hardcoded responses to stdout. Stays alive
/// until stdin closes, at which point it exits cleanly (mirroring how a real
/// ACP agent terminates when the client drops the connection).
///
/// Recognizes:
/// - `initialize` -> protocolVersion = 1, agentCapabilities with
///   `loadSession` and `sessionCapabilities.{resume,close}` set unless the
///   matching `--no-cap-*` flag is supplied, agentInfo set.
/// - `session/new` -> deterministic `sess_fake_{counter}` id.
/// - `session/load`, `session/resume`, `session/close` -> empty ok.
/// - `session/prompt` -> emits two `session/update` notifications with agent
///   message chunks, then returns `{ stopReason: "end_turn" }`. With
///   `--prompt-cancel` it returns `{ stopReason: "cancelled" }` instead.
/// - `--initialize-error`, `--session-new-error`, and `--prompt-error` return
///   JSON-RPC errors for their matching request stages.
/// - `--session-new-stall` accepts initialize but never answers `session/new`.
/// - `--model-config-option <value>` advertises that value as a model session
///   config option.
/// - `--write-pid <path>` writes the fake agent's process id for cleanup tests.
/// - any other request -> JSON-RPC method-not-found.
///
/// `session/cancel` notification flips the in-process flag so the next
/// `session/prompt` response uses `stopReason: cancelled`.
///
/// Compiled only into debug builds — release binaries do not expose this code
/// path.
#[cfg(debug_assertions)]
pub(super) fn run_fake_agent(args: Vec<String>) -> Result<()> {
    use std::io::{BufRead, Write};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let mut title = "acps fake agent".to_owned();
    let mut load_session_cap = true;
    let mut resume_session_cap = true;
    let mut close_session_cap = true;
    let mut prompt_emits_updates = true;
    let mut initialize_fails = false;
    let mut session_new_fails = false;
    let mut session_new_stalls = false;
    let mut prompt_fails = false;
    let mut prompt_stalls_after_update = false;
    let mut model_config_option: Option<String> = None;
    let mut expected_model_config: Option<String> = None;
    let mut pid_path: Option<String> = None;
    let mut env_assertions = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--assert-env-absent" => {
                if let Some(name) = iter.next()
                    && std::env::var_os(name).is_some()
                {
                    env_assertions.push(format!("env leaked: {name}"));
                }
            }
            "--assert-env-present" => {
                if let Some(name) = iter.next()
                    && std::env::var_os(name).is_none()
                {
                    env_assertions.push(format!("env missing: {name}"));
                }
            }
            "--assert-env-not-equals" => {
                if let (Some(name), Some(value)) = (iter.next(), iter.next())
                    && std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new(value))
                {
                    env_assertions.push(format!("env override: {name}"));
                }
            }
            "--no-cap-load-session" => {
                load_session_cap = false;
            }
            "--no-cap-resume-session" => {
                resume_session_cap = false;
            }
            "--no-cap-close-session" => {
                close_session_cap = false;
            }
            "--prompt-silent" => {
                prompt_emits_updates = false;
            }
            "--initialize-error" => {
                initialize_fails = true;
            }
            "--session-new-error" => {
                session_new_fails = true;
            }
            "--session-new-stall" => {
                session_new_stalls = true;
            }
            "--prompt-error" => {
                prompt_fails = true;
            }
            "--prompt-stall-after-update" => {
                prompt_stalls_after_update = true;
            }
            "--model-config-option" => {
                if let Some(value) = iter.next() {
                    model_config_option = Some(value.to_owned());
                }
            }
            "--expect-model-config" => {
                if let Some(value) = iter.next() {
                    expected_model_config = Some(value.to_owned());
                }
            }
            "--write-pid" => {
                if let Some(value) = iter.next() {
                    pid_path = Some(value.to_owned());
                }
            }
            _ => {}
        }
    }
    if let Some(path) = pid_path {
        std::fs::write(path, std::process::id().to_string())
            .map_err(|source| StackError::ServeIo { source })?;
    }
    if title == "acps fake agent" && args.iter().any(|arg| arg.starts_with("--assert-env-")) {
        title = if env_assertions.is_empty() {
            "env assertions passed".to_owned()
        } else {
            env_assertions.join(", ")
        };
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdin = stdin.lock();
    let stdout_handle = std::sync::Mutex::new(stdout.lock());
    let session_counter = AtomicU64::new(0);
    let cancel_requested = AtomicBool::new(false);
    let model_configured = AtomicBool::new(false);
    let mut buf = String::new();
    loop {
        buf.clear();
        match stdin.read_line(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(source) => return Err(StackError::StdinRead { source }),
        }
        let trimmed = buf.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = value.get("id");
        let method = value.get("method").and_then(serde_json::Value::as_str);
        // Notifications have no id; act on the ones we recognize and ignore
        // anything else. `session/cancel` toggles the flag so the next
        // `session/prompt` response returns `cancelled`.
        let Some(id) = id else {
            if method == Some("session/cancel") {
                cancel_requested.store(true, Ordering::SeqCst);
            }
            continue;
        };
        let response = match method {
            Some("initialize") => {
                if initialize_fails {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": "fake initialize failure"
                        }
                    })
                } else {
                    let mut agent_caps = serde_json::Map::new();
                    agent_caps.insert(
                        "loadSession".to_owned(),
                        serde_json::Value::Bool(load_session_cap),
                    );
                    let mut session_caps = serde_json::Map::new();
                    if resume_session_cap {
                        session_caps.insert("resume".to_owned(), serde_json::json!({}));
                    }
                    if close_session_cap {
                        session_caps.insert("close".to_owned(), serde_json::json!({}));
                    }
                    agent_caps.insert(
                        "sessionCapabilities".to_owned(),
                        serde_json::Value::Object(session_caps),
                    );
                    agent_caps.insert("promptCapabilities".to_owned(), serde_json::json!({}));
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "agentCapabilities": agent_caps,
                            "agentInfo": {
                                "name": "acps-fake-agent",
                                "title": title,
                                "version": "0.0.1"
                            },
                            "authMethods": []
                        }
                    })
                }
            }
            Some("session/new") => {
                if session_new_fails {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": "fake session/new failure"
                        }
                    })
                } else if session_new_stalls {
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                } else {
                    let counter = session_counter.fetch_add(1, Ordering::SeqCst);
                    let mut result = serde_json::json!({
                        "sessionId": format!("sess_fake_{counter}")
                    });
                    if let Some(model) = model_config_option.as_deref() {
                        result["configOptions"] = serde_json::json!([
                            {
                                "id": "model",
                                "name": "Model",
                                "category": "model",
                                "type": "select",
                                "currentValue": model,
                                "options": [
                                    { "value": model, "name": model }
                                ]
                            }
                        ]);
                    }
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    })
                }
            }
            Some("session/set_config_option") => {
                let config_id = value
                    .get("params")
                    .and_then(|p| p.get("configId"))
                    .and_then(|v| v.as_str());
                let config_value = value
                    .get("params")
                    .and_then(|p| p.get("value"))
                    .and_then(|v| v.as_str());
                if let Some(expected) = expected_model_config.as_deref()
                    && config_id == Some("model")
                    && config_value == Some(expected)
                {
                    model_configured.store(true, Ordering::SeqCst);
                }
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "configOptions": [] }
                })
            }
            Some("session/load") | Some("session/resume") | Some("session/close") => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                })
            }
            Some("session/prompt") => {
                if prompt_fails {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": "fake prompt failure"
                        }
                    })
                } else if expected_model_config.is_some()
                    && !model_configured.load(Ordering::SeqCst)
                {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": "expected model config before prompt"
                        }
                    })
                } else {
                    let session_id = value
                        .get("params")
                        .and_then(|p| p.get("sessionId"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("sess_fake_unknown")
                        .to_owned();
                    if value.to_string().contains(FAKE_AGENT_TESTFLIGHT_MARKER) {
                        std::fs::write(FAKE_AGENT_TESTFLIGHT_MARKER, FAKE_AGENT_TESTFLIGHT_CONTENT)
                            .map_err(|source| StackError::ServeIo { source })?;
                    }
                    if prompt_emits_updates {
                        // Push two notifications before the response. The bridge's
                        // SessionEventSink persists them keyed by session_id; tests
                        // assert on the `events.session_id` column.
                        let chunks: &[&str] = if prompt_stalls_after_update {
                            &["chunk-1"]
                        } else {
                            &["chunk-1", "chunk-2"]
                        };
                        for text in chunks {
                            let note = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": session_id,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": text }
                                    }
                                }
                            });
                            let mut guard = stdout_handle
                                .lock()
                                .expect("fake-agent stdout mutex poisoned");
                            writeln!(*guard, "{note}")
                                .map_err(|source| StackError::ServeIo { source })?;
                            guard
                                .flush()
                                .map_err(|source| StackError::ServeIo { source })?;
                        }
                    }
                    if prompt_stalls_after_update {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                        continue;
                    }
                    let stop = if cancel_requested.swap(false, Ordering::SeqCst) {
                        "cancelled"
                    } else {
                        "end_turn"
                    };
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "stopReason": stop }
                    })
                }
            }
            _ => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": "method not found"
                }
            }),
        };
        let mut guard = stdout_handle
            .lock()
            .expect("fake-agent stdout mutex poisoned");
        writeln!(*guard, "{response}").map_err(|source| StackError::ServeIo { source })?;
        guard
            .flush()
            .map_err(|source| StackError::ServeIo { source })?;
    }
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
