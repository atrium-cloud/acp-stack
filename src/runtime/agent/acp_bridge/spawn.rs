//! Agent process launch: command resolution, child setup, ACP handler
//! registration, and the `initialize` handshake that produces a live
//! [`AcpBridge`].

use tokio::process::{ChildStdin, ChildStdout};

use super::*;

/// Exit-reporting handles shared by the connection task and the child exit
/// watcher.
#[derive(Clone)]
struct ExitReporter {
    tx: watch::Sender<Option<AcpBridgeExit>>,
    planned_shutdown: Arc<AtomicBool>,
    pid: Option<u32>,
}

/// The connection task plus the endpoints the spawning path keeps.
struct ConnectionTask {
    task: JoinHandle<()>,
    connection_rx: oneshot::Receiver<InitializeOutcome>,
    shutdown_tx: oneshot::Sender<()>,
}

type InitializeOutcome = std::result::Result<(InitializeResponse, ConnectionTo<Agent>), String>;

impl AcpBridge {
    /// Spawn `[agent].command` and complete the ACP `initialize` handshake.
    /// The command path resolves before `env_clear()`, so only managed runtime
    /// context and explicitly resolved secrets ever reach the child.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        home: &Path,
        agent: &AgentConfig,
        env: HashMap<String, String>,
        cwd: PathBuf,
        sink: Arc<dyn SessionEventSink>,
        permissions: AcpPermissionPolicy,
        sandbox: &crate::config::SandboxConfig,
        shell: &str,
        network_provider: Option<&crate::extensions::NetworkProviderExtension>,
        command_log: Option<TerminalCommandLog>,
    ) -> Result<Self> {
        let mut env = build_agent_process_env(agent, home, env)?;
        // Last write wins, so the namespace owner's declaration overrides both
        // `[agent].env` and the runtime-managed rewrites above it.
        crate::extensions::apply_workload_env(&mut env, network_provider);
        let wrapped = wrap_agent_command(agent, &cwd, sandbox, network_provider, home)?;
        let command = build_agent_command(&wrapped, &cwd, &env, home);
        let (mut child, stdin, stdout) =
            spawn_agent_child(command, &wrapped, sandbox, network_provider)?;

        let (exit_tx, exit_rx) = watch::channel(None);
        let exit = ExitReporter {
            tx: exit_tx,
            planned_shutdown: Arc::new(AtomicBool::new(false)),
            pid: child.id(),
        };

        let notification_drain = Arc::new(NotificationDrain::default());
        let terminals = Arc::new(TerminalRegistry::default());
        let terminal_context = Arc::new(TerminalHandlerContext {
            registry: Arc::clone(&terminals),
            workspace_root: cwd.clone(),
            home: home.to_path_buf(),
            sandbox: sandbox.clone(),
            shell: shell.to_owned(),
            network_provider: network_provider.cloned(),
            command_log,
            sink: sink.clone(),
        });

        let ConnectionTask {
            task,
            connection_rx,
            shutdown_tx,
        } = spawn_connection_task(
            stdin,
            stdout,
            permissions,
            sink.clone(),
            Arc::clone(&notification_drain),
            terminal_context,
            exit.clone(),
        );

        let (capabilities, connection, task) =
            complete_initialize(&mut child, task, connection_rx).await?;

        let child = Arc::new(TokioMutex::new(Some(child)));
        spawn_child_exit_watcher(Arc::clone(&child), exit.clone());

        Ok(Self {
            child,
            capabilities,
            connection: TokioMutex::new(Some(connection)),
            shutdown_tx: TokioMutex::new(Some(shutdown_tx)),
            connection_task: TokioMutex::new(Some(task)),
            planned_shutdown: exit.planned_shutdown,
            exit_rx,
            spawn_pid: exit.pid,
            sink,
            notification_drain,
            terminals,
        })
    }
}

/// Resolve `[agent].command` against PATH/cwd and apply the sandbox wrapper.
fn wrap_agent_command(
    agent: &AgentConfig,
    cwd: &Path,
    sandbox: &crate::config::SandboxConfig,
    network_provider: Option<&crate::extensions::NetworkProviderExtension>,
    home: &Path,
) -> Result<crate::runtime::sandbox::WrappedCommand> {
    let command_path = resolve_command_path(&agent.command, cwd, home).ok_or_else(|| {
        StackError::AgentInitializeFailed {
            reason: format!("agent command `{}` not found on PATH", agent.command),
        }
    })?;
    // `off` is a verbatim passthrough so single-process behavior is unchanged;
    // other modes wrap the spawn.
    if matches!(sandbox.mode, crate::config::SandboxMode::Off) {
        Ok(crate::runtime::sandbox::WrappedCommand {
            program: command_path,
            args: agent.args.clone(),
        })
    } else {
        crate::runtime::sandbox::wrap(
            sandbox,
            network_provider,
            &command_path,
            &agent.args,
            home,
            cwd,
            crate::ownership::process_euid(),
            crate::ownership::process_egid(),
        )
    }
}

fn build_agent_command(
    wrapped: &crate::runtime::sandbox::WrappedCommand,
    cwd: &Path,
    env: &HashMap<String, String>,
    home: &Path,
) -> Command {
    let mut command = Command::new(&wrapped.program);
    command
        .args(&wrapped.args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // stderr is the agent's ACP log channel, so inherit it into the daemon
        // logs.
        .stderr(std::process::Stdio::inherit())
        .env_clear();
    // Runtime context is deliberately narrow: managed PATH for
    // registry-installed harnesses, HOME for agent config/cache directories.
    // Both derive from the caller's boot-time home, never the process env at
    // spawn time.
    if let Some(path) = agent_process_path(home) {
        command.env("PATH", path);
    }
    command.env("HOME", home);
    for (name, value) in env {
        if matches!(name.as_str(), "PATH" | "HOME") {
            tracing::warn!(
                name = %name,
                "refusing to inject `{name}` from `[agent].env` into agent process: reserved",
            );
            continue;
        }
        command.env(name, value);
    }
    // Fresh process group so SIGTERM-during-shutdown also reaches MCP/tool
    // grandchildren the agent forks.
    #[cfg(unix)]
    command.process_group(0);
    command.kill_on_drop(true);
    command
}

fn spawn_agent_child(
    mut command: Command,
    wrapped: &crate::runtime::sandbox::WrappedCommand,
    sandbox: &crate::config::SandboxConfig,
    network_provider: Option<&crate::extensions::NetworkProviderExtension>,
) -> Result<(Child, ChildStdin, ChildStdout)> {
    // Network-isolated spawns get the daemon's stderr at the supervisor's
    // diagnostic fd; a no-op for every other mode.
    #[cfg(unix)]
    let diag_handle = crate::runtime::sandbox::wire_supervise_diag_fd(
        sandbox,
        network_provider,
        &mut command,
        &wrapped.args,
    )
    .map_err(|source| StackError::AgentSpawnFailed { source })?;

    let spawn_result = command.spawn();
    #[cfg(unix)]
    drop(diag_handle);
    let mut child = spawn_result.map_err(|source| StackError::AgentSpawnFailed { source })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "agent stdin was not piped".to_owned(),
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "agent stdout was not piped".to_owned(),
        })?;
    Ok((child, stdin, stdout))
}

/// Register the client-side ACP handlers and drive the connection.
fn spawn_connection_task(
    stdin: ChildStdin,
    stdout: ChildStdout,
    permissions: AcpPermissionPolicy,
    sink: Arc<dyn SessionEventSink>,
    notification_drain: Arc<NotificationDrain>,
    terminal_context: Arc<TerminalHandlerContext>,
    exit: ExitReporter,
) -> ConnectionTask {
    let transport = agent_client_protocol::ByteStreams::new(stdin.compat_write(), stdout.compat());
    let (init_tx, connection_rx) = oneshot::channel::<InitializeOutcome>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let notification_queue = spawn_session_notification_queue(sink.clone());
    let permission_sink = sink;
    let create_context = Arc::clone(&terminal_context);
    let output_context = Arc::clone(&terminal_context);
    let wait_context = Arc::clone(&terminal_context);
    let kill_context = Arc::clone(&terminal_context);
    let release_context = Arc::clone(&terminal_context);
    let fs_read_context = Arc::clone(&terminal_context);
    let fs_write_context = terminal_context;

    let task: JoinHandle<()> = tokio::spawn(async move {
        let run = Client
            .builder()
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, cx| {
                    let permissions = permissions.clone();
                    let sink = permission_sink.clone();
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let outcome = match &permissions {
                            AcpPermissionPolicy::Service(service) => {
                                resolve_acp_permission(service, &sink, request, Some(cancellation))
                                    .await
                            }
                            AcpPermissionPolicy::AutoApprove => {
                                Ok(auto_approve_acp_permission(&request))
                            }
                            AcpPermissionPolicy::Cancel => Ok(RequestPermissionOutcome::Cancelled),
                        };
                        match outcome {
                            Ok(outcome) => responder.respond(RequestPermissionResponse::new(outcome)),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            // Terminal handlers must offload to spawned tasks: they can park a
            // long time and handler callbacks run on the connection's single
            // event loop, which has to keep serving concurrent calls.
            .on_receive_request(
                async move |request: CreateTerminalRequest, responder, cx| {
                    let context = Arc::clone(&create_context);
                    cx.spawn(async move {
                        match handle_create_terminal(&context, request).await {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: TerminalOutputRequest, responder, cx| {
                    let context = Arc::clone(&output_context);
                    cx.spawn(async move {
                        match handle_terminal_output(&context.registry, request).await {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: WaitForTerminalExitRequest, responder, cx| {
                    let context = Arc::clone(&wait_context);
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        tokio::select! {
                            result = handle_wait_for_terminal_exit(&context.registry, request) => {
                                match result {
                                    Ok(response) => responder.respond(response),
                                    Err(error) => responder.respond_with_error(error),
                                }
                            }
                            () = cancellation.cancelled() => {
                                responder.respond_with_error(agent_client_protocol::Error::request_cancelled())
                            }
                        }
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: KillTerminalRequest, responder, cx| {
                    let context = Arc::clone(&kill_context);
                    cx.spawn(async move {
                        match handle_kill_terminal(&context.registry, request).await {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: ReleaseTerminalRequest, responder, cx| {
                    let context = Arc::clone(&release_context);
                    cx.spawn(async move {
                        match handle_release_terminal(&context.registry, request).await {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: ReadTextFileRequest, responder, cx| {
                    let context = Arc::clone(&fs_read_context);
                    cx.spawn(async move {
                        match handle_read_text_file(&context.workspace_root, &context.sink, request)
                            .await
                        {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: WriteTextFileRequest, responder, cx| {
                    let context = Arc::clone(&fs_write_context);
                    cx.spawn(async move {
                        match handle_write_text_file(
                            &context.workspace_root,
                            context.command_log.as_ref().map(|log| &log.state),
                            &context.sink,
                            request,
                        )
                        .await
                        {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |notification: AgentNotification, _cx| {
                    match notification {
                        AgentNotification::SessionNotification(session_note) => {
                            enqueue_session_notification(
                                &notification_queue,
                                Arc::clone(&notification_drain),
                                session_note,
                            )
                            .await;
                        }
                        other => {
                            tracing::debug!(
                                method = %other.method(),
                                "acp bridge dropped non-session notification"
                            );
                        }
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                let response = cx
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(client_capabilities())
                            .client_info(Implementation::new("acp-stack", env!("CARGO_PKG_VERSION"))),
                    )
                    .block_task()
                    .await
                    .map_err(|err| err.to_string());
                match response {
                    Ok(response) => {
                        // Hand the connection out so the bridge can dispatch
                        // session methods after this closure parks.
                        let _ = init_tx.send(Ok((response, cx.clone())));
                    }
                    Err(reason) => {
                        let _ = init_tx.send(Err(reason));
                        // Returning an error tears the connection down; the
                        // caller already saw the failure via the oneshot.
                        return Err(agent_client_protocol::Error::internal_error());
                    }
                }
                // A dropped shutdown sender also means "tear down now".
                let _ = shutdown_rx.await;
                Ok(())
            })
            .await;
        let planned = exit.planned_shutdown.load(Ordering::SeqCst);
        let bridge_exit = match run {
            Ok(()) if planned => AcpBridgeExit {
                pid: exit.pid,
                planned,
                reason: AcpBridgeExitReason::Shutdown,
                message: None,
                exit_status: None,
            },
            Ok(()) => AcpBridgeExit {
                pid: exit.pid,
                planned,
                reason: AcpBridgeExitReason::ConnectionEnded,
                message: None,
                exit_status: None,
            },
            Err(err) => {
                tracing::warn!(error = ?err, "acp bridge connection task exited with error");
                AcpBridgeExit {
                    pid: exit.pid,
                    planned,
                    reason: AcpBridgeExitReason::ConnectionError,
                    message: Some(err.to_string()),
                    exit_status: None,
                }
            }
        };
        let _ = exit.tx.send(Some(bridge_exit));
    });

    ConnectionTask {
        task,
        connection_rx,
        shutdown_tx,
    }
}

/// Await the handshake result, validate the protocol version, and snapshot the
/// agent's capabilities. Every failure path MUST tear the spawned child down
/// and consume `connection_task`; only the success path hands it back.
async fn complete_initialize(
    child: &mut Child,
    connection_task: JoinHandle<()>,
    connection_rx: oneshot::Receiver<InitializeOutcome>,
) -> Result<(AgentCapabilitiesDto, ConnectionTo<Agent>, JoinHandle<()>)> {
    let (init_response, connection) = match timeout(INITIALIZE_TIMEOUT, connection_rx).await {
        Ok(Ok(Ok((response, connection)))) => (response, connection),
        Ok(Ok(Err(reason))) => {
            fail_spawn(child, connection_task).await;
            return Err(StackError::AgentInitializeFailed { reason });
        }
        Ok(Err(_)) => {
            fail_spawn(child, connection_task).await;
            return Err(StackError::AgentInitializeFailed {
                reason: "connection ended before initialize completed".to_owned(),
            });
        }
        Err(_) => {
            fail_spawn(child, connection_task).await;
            return Err(StackError::AgentInitializeFailed {
                reason: format!(
                    "initialize did not return within {}s",
                    INITIALIZE_TIMEOUT.as_secs()
                ),
            });
        }
    };

    if init_response.protocol_version != ProtocolVersion::V1 {
        let returned = init_response.protocol_version.as_u16();
        fail_spawn(child, connection_task).await;
        return Err(StackError::AgentInitializeFailed {
            reason: format!(
                "requested ACP protocol version {} but agent returned {returned}",
                ProtocolVersion::V1.as_u16()
            ),
        });
    }
    let capabilities = match AgentCapabilitiesDto::from_initialize_response(&init_response) {
        Ok(capabilities) => capabilities,
        Err(error) => {
            fail_spawn(child, connection_task).await;
            return Err(error);
        }
    };
    Ok((capabilities, connection, connection_task))
}

/// Capabilities advertised to every agent at initialize. A flag flips only
/// once its agent->client handlers exist, since advertising ahead of them
/// invites calls we cannot serve.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(true))
        .terminal(true)
        .session(ClientSessionCapabilities::new().config_options(
            SessionConfigOptionsCapabilities::new().boolean(BooleanConfigOptionCapabilities::new()),
        ))
}

fn spawn_child_exit_watcher(child: Arc<TokioMutex<Option<Child>>>, exit: ExitReporter) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CHILD_EXIT_POLL_INTERVAL).await;
            let exit_status = {
                let mut guard = child.lock().await;
                let Some(child) = guard.as_mut() else {
                    return;
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *guard = None;
                        Some(status.code())
                    }
                    Ok(None) => None,
                    Err(err) => {
                        tracing::warn!(error = ?err, "acp bridge child exit poll failed");
                        None
                    }
                }
            };
            let Some(exit_status) = exit_status else {
                continue;
            };
            let planned = exit.planned_shutdown.load(Ordering::SeqCst);
            let _ = exit.tx.send(Some(AcpBridgeExit {
                pid: exit.pid,
                planned,
                reason: AcpBridgeExitReason::ProcessExited,
                message: None,
                exit_status,
            }));
            return;
        }
    });
}

/// Spawn-error cleanup: abort the SDK task, kill the whole process group, then
/// reap. The pgroup kill is required — without it, grandchildren forked between
/// spawn and initialize-failure survive.
async fn fail_spawn(child: &mut Child, connection_task: JoinHandle<()>) {
    connection_task.abort();
    let _ = connection_task.await;
    kill_tokio_process_group(child);
    let _ = child.wait().await;
}

/// Resolve a configured command path the same way process spawning will.
pub(crate) fn resolve_command_path(command: &str, cwd: &Path, home: &Path) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    let as_path = Path::new(command);
    if as_path.is_absolute() {
        return if as_path.is_file() {
            Some(as_path.to_path_buf())
        } else {
            None
        };
    }
    if command.contains('/') {
        let candidate = cwd.join(command);
        return if candidate.is_file() {
            Some(candidate)
        } else {
            None
        };
    }
    for dir in command_search_paths(home) {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn agent_process_path(home: &Path) -> Option<std::ffi::OsString> {
    let paths = command_search_paths(home);
    if paths.is_empty() {
        None
    } else {
        std::env::join_paths(paths).ok()
    }
}

fn command_search_paths(home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.push(home.join(".local").join("bin"));
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    paths
}
