use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, StopReason, TextContent};
use tokio::sync::Notify;

use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::runtime::agent::acp_bridge::{
    AcpBridge, AgentSessionConfigCategory, AgentSessionModelSelection, SessionEventSink,
    session_config_id_for_value, session_model_selection_for_value,
};
use crate::runtime::install::agent_registry::RegistryCatalog;

use super::install::{operator_registry_override, resolve_agent_env_for_cli};
use super::{
    AgentTestArgs, DEFAULT_AGENT_TEST_PROGRESS_TIMEOUT, DEFAULT_AGENT_TEST_PROMPT,
    DEFAULT_AGENT_TEST_TIMEOUT,
};

struct AgentTestSessionEventSink {
    updates: AtomicUsize,
    notify: Notify,
}

impl AgentTestSessionEventSink {
    fn new() -> Self {
        Self {
            updates: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    fn update_count(&self) -> usize {
        self.updates.load(Ordering::SeqCst)
    }

    async fn wait_for_update_after(&self, observed_updates: usize) {
        loop {
            if self.update_count() > observed_updates {
                return;
            }
            self.notify.notified().await;
        }
    }
}

impl SessionEventSink for AgentTestSessionEventSink {
    fn append<'a>(
        &'a self,
        _session_id: &'a str,
        kind: &'a str,
        _payload_json: &'a str,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            if kind == "session.update" {
                self.updates.fetch_add(1, Ordering::SeqCst);
                self.notify.notify_waiters();
            }
        })
    }
}

struct AgentTestReport {
    session_id: String,
    stop_reason: StopReason,
    updates: usize,
}

/// Run a real-prompt testflight at the tail of `acps init`. Uses the registry
/// entry's `testflight_prompt` if present (else the default) and verifies the
/// declared `testflight_expect_fs` artifact post-prompt. Surfaces the same
/// "ok / session_id / stop_reason / updates / fs_check" lines as
/// `acps agent test` so the operator sees consistent output regardless of
/// which entry point they used.
pub(in crate::cli) fn run_init_testflight(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    print_summary: bool,
) -> Result<()> {
    let args = AgentTestArgs {
        prompt: None,
        timeout: DEFAULT_AGENT_TEST_TIMEOUT.to_owned(),
        progress_timeout: DEFAULT_AGENT_TEST_PROGRESS_TIMEOUT.to_owned(),
    };
    run_agent_test_with(home, config, registry, args, print_summary)
}

pub(super) fn run_agent_test(args: AgentTestArgs) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    run_agent_test_with(&home, &config, &registry, args, true)
}

fn run_agent_test_with(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    args: AgentTestArgs,
    print_summary: bool,
) -> Result<()> {
    let entry = registry.lookup_required(&config.agent.id)?;
    entry.ensure_supported()?;

    let prompt_source = if args.prompt.is_some() {
        AgentTestPromptSource::CliFlag
    } else if entry.testflight_prompt.is_some() {
        AgentTestPromptSource::Registry
    } else {
        AgentTestPromptSource::Default
    };
    let prompt = args
        .prompt
        .clone()
        .or_else(|| entry.testflight_prompt.clone())
        .unwrap_or_else(|| DEFAULT_AGENT_TEST_PROMPT.to_owned());
    let expect_fs = match prompt_source {
        AgentTestPromptSource::Registry => entry.testflight_expect_fs.clone(),
        AgentTestPromptSource::CliFlag | AgentTestPromptSource::Default => None,
    };
    let workspace_root = PathBuf::from(&config.workspace.root);
    let timeout = parse_agent_test_duration("agent test --timeout", &args.timeout)?;
    let progress_timeout =
        parse_agent_test_duration("agent test --progress-timeout", &args.progress_timeout)?;
    let env = resolve_agent_env_for_cli(home, config)?;
    let cwd = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));
    let agent = config.agent.clone();

    if let Some(rel) = expect_fs.as_deref() {
        prepare_testflight_expect_fs(&workspace_root, rel)?;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    let sandbox = config.workspace.sandbox.clone();
    let network_provider = crate::extensions::resolve_network_provider(config);
    let report = runtime.block_on(async move {
        run_agent_test_inner(
            agent,
            env,
            cwd,
            prompt,
            timeout,
            progress_timeout,
            sandbox,
            network_provider,
        )
        .await
    })?;

    let fs_outcome = match expect_fs.as_deref() {
        Some(rel) => Some(verify_testflight_expect_fs(&workspace_root, rel)?),
        None => None,
    };

    if print_summary {
        println!("agent test: ok");
        println!("agent: {}", config.agent.id);
        println!("prompt: {}", prompt_source.label());
        println!("session_id: {}", report.session_id);
        println!("stop_reason: {}", stop_reason_label(report.stop_reason));
        println!("updates: {}", report.updates);
        if let Some(outcome) = fs_outcome {
            println!(
                "fs_check: ok ({} bytes at {})",
                outcome.bytes,
                outcome.path.display()
            );
        }
    }
    Ok(())
}

#[derive(Copy, Clone)]
enum AgentTestPromptSource {
    CliFlag,
    Registry,
    Default,
}

impl AgentTestPromptSource {
    fn label(self) -> &'static str {
        match self {
            AgentTestPromptSource::CliFlag => "provided",
            AgentTestPromptSource::Registry => "registry",
            AgentTestPromptSource::Default => "default",
        }
    }
}

#[derive(Debug)]
pub(super) struct TestflightFsOutcome {
    pub(super) path: PathBuf,
    pub(super) bytes: u64,
}

/// Verify the registry-declared testflight artifact lives under the workspace
/// after the prompt completes. Treats absence and zero-length files as
/// failures so the operator can distinguish "agent did not run the tool"
/// from "agent ran the tool successfully". Uses canonical paths to reject
/// an agent that resolved a symlink out of the workspace.
pub(super) fn prepare_testflight_expect_fs(workspace_root: &Path, relative: &str) -> Result<()> {
    let path = testflight_expect_fs_path(workspace_root, relative)?;
    ensure_testflight_parent_within_workspace(workspace_root, &path)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::remove_file(&path).map_err(|source| StackError::AgentTestFailed {
                stage: "fs_check".to_owned(),
                reason: format!(
                    "remove stale testflight artifact `{}` failed: {source}",
                    path.display()
                ),
            })?;
            Ok(())
        }
        Ok(metadata) => Err(StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "pre-existing testflight artifact `{}` is {}; remove it before running testflight",
                path.display(),
                if metadata.file_type().is_symlink() {
                    "a symlink"
                } else {
                    "not a regular file"
                }
            ),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "stat pre-existing testflight artifact `{}` failed: {source}",
                path.display()
            ),
        }),
    }
}

pub(super) fn verify_testflight_expect_fs(
    workspace_root: &Path,
    relative: &str,
) -> Result<TestflightFsOutcome> {
    let path = testflight_expect_fs_path(workspace_root, relative)?;
    let workspace =
        workspace_root
            .canonicalize()
            .map_err(|source| StackError::AgentTestFailed {
                stage: "fs_check".to_owned(),
                reason: format!(
                    "canonicalize workspace root `{}` failed: {source}",
                    workspace_root.display()
                ),
            })?;
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|source| StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "expected agent to create `{}` (workspace-relative `{}`) but stat failed: {source}",
                path.display(),
                relative
            ),
        })?;
    if metadata.file_type().is_symlink() {
        return Err(StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "expected agent to create regular file `{}`, but it is a symlink",
                path.display()
            ),
        });
    }
    if !metadata.is_file() {
        return Err(StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "expected agent to create regular file `{}`, but it is not a regular file",
                path.display()
            ),
        });
    }
    let canonical_path = path
        .canonicalize()
        .map_err(|source| StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "canonicalize testflight artifact `{}` failed: {source}",
                path.display()
            ),
        })?;
    if !canonical_path.starts_with(&workspace) {
        return Err(StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "testflight artifact `{}` resolved outside workspace `{}`",
                canonical_path.display(),
                workspace.display()
            ),
        });
    }
    if metadata.len() == 0 {
        return Err(StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "agent created `{}` but the file is empty; treating as no tool action",
                path.display()
            ),
        });
    }
    Ok(TestflightFsOutcome {
        path,
        bytes: metadata.len(),
    })
}

fn testflight_expect_fs_path(workspace_root: &Path, relative: &str) -> Result<PathBuf> {
    if Path::new(relative).is_absolute() || relative.split('/').any(|seg| seg == "..") {
        return Err(StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "testflight_expect_fs `{relative}` must be a workspace-relative path with no `..` segments"
            ),
        });
    }
    Ok(workspace_root.join(relative))
}

fn ensure_testflight_parent_within_workspace(workspace_root: &Path, path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let workspace =
        workspace_root
            .canonicalize()
            .map_err(|source| StackError::AgentTestFailed {
                stage: "fs_check".to_owned(),
                reason: format!(
                    "canonicalize workspace root `{}` failed: {source}",
                    workspace_root.display()
                ),
            })?;
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StackError::AgentTestFailed {
                stage: "fs_check".to_owned(),
                reason: format!("canonicalize `{}` failed: {source}", parent.display()),
            });
        }
    };
    if parent.starts_with(&workspace) {
        Ok(())
    } else {
        Err(StackError::AgentTestFailed {
            stage: "fs_check".to_owned(),
            reason: format!(
                "testflight artifact parent `{}` resolved outside workspace `{}`",
                parent.display(),
                workspace.display()
            ),
        })
    }
}

fn parse_agent_test_duration(field: &'static str, value: &str) -> Result<Duration> {
    let duration =
        config::parse_duration_string(value).ok_or(StackError::InvalidDurationField { field })?;
    if duration.is_zero() {
        return Err(StackError::InvalidDurationField { field });
    }
    Ok(duration)
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_test_inner(
    agent: crate::config::AgentConfig,
    env: HashMap<String, String>,
    cwd: PathBuf,
    prompt: String,
    prompt_timeout: Duration,
    progress_timeout: Duration,
    sandbox: crate::config::SandboxConfig,
    network_provider: Option<crate::extensions::NetworkProviderExtension>,
) -> Result<AgentTestReport> {
    let sink = Arc::new(AgentTestSessionEventSink::new());
    let bridge = AcpBridge::spawn(
        &agent,
        env,
        cwd.clone(),
        sink.clone(),
        None,
        &sandbox,
        network_provider.as_ref(),
        None,
    )
    .await
    .map_err(agent_test_spawn_error)?;

    let result = async {
        let session = bridge
            .new_session(cwd, Vec::new())
            .await
            .map_err(|err| agent_test_error("session creation", err))?;
        apply_agent_test_session_config(&bridge, &agent, &session)
            .await
            .map_err(|err| agent_test_error("session creation", err))?;
        let request = PromptRequest::new(
            session.session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        let stop_reason = run_agent_test_prompt(
            &bridge,
            request,
            sink.clone(),
            prompt_timeout,
            progress_timeout,
        )
        .await?;
        if stop_reason != StopReason::EndTurn {
            return Err(StackError::AgentTestFailed {
                stage: "prompt completion".to_owned(),
                reason: format!(
                    "expected stop_reason end_turn, got {}",
                    stop_reason_label(stop_reason)
                ),
            });
        }
        Ok(AgentTestReport {
            session_id: session.session_id.to_string(),
            stop_reason,
            updates: sink.update_count(),
        })
    }
    .await;

    let shutdown = bridge.shutdown().await;
    match (result, shutdown) {
        (Ok(report), Ok(_)) => Ok(report),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(agent_test_error("shutdown", err)),
    }
}

async fn run_agent_test_prompt(
    bridge: &AcpBridge,
    request: PromptRequest,
    sink: Arc<AgentTestSessionEventSink>,
    prompt_timeout: Duration,
    progress_timeout: Duration,
) -> Result<StopReason> {
    let prompt_call = async {
        let prompt_future = bridge.prompt_session(request);
        tokio::pin!(prompt_future);
        let mut observed_updates = sink.update_count();
        loop {
            let progress_timer = tokio::time::sleep(progress_timeout);
            tokio::pin!(progress_timer);
            tokio::select! {
                result = &mut prompt_future => {
                    return result
                        .map(|response| response.stop_reason)
                        .map_err(|err| agent_test_error("prompt completion", err));
                }
                _ = sink.wait_for_update_after(observed_updates) => {
                    observed_updates = sink.update_count();
                }
                _ = &mut progress_timer => {
                    return Err(StackError::AgentTestFailed {
                        stage: "prompt/progress timeout".to_owned(),
                        reason: format!(
                            "no new session/update or terminal prompt response within {}",
                            human_duration(progress_timeout)
                        ),
                    });
                }
            }
        }
    };

    tokio::time::timeout(prompt_timeout, prompt_call)
        .await
        .map_err(|_| StackError::AgentTestFailed {
            stage: "prompt/progress timeout".to_owned(),
            reason: format!(
                "prompt did not complete within {}",
                human_duration(prompt_timeout)
            ),
        })?
}

async fn apply_agent_test_session_config(
    bridge: &AcpBridge,
    agent: &crate::config::AgentConfig,
    response: &agent_client_protocol::schema::v1::NewSessionResponse,
) -> Result<()> {
    if let Some(mode) = agent.mode.as_deref() {
        let config_id = session_config_id_for_value(
            response.config_options.as_deref(),
            AgentSessionConfigCategory::Mode,
            mode,
        )?;
        bridge
            .set_session_config_option(response.session_id.clone(), &config_id, mode)
            .await?;
    }
    if let Some(model) = agent.model.as_deref().or_else(|| {
        agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
    }) {
        let AgentSessionModelSelection::ConfigOption { config_id } =
            session_model_selection_for_value(response, model)?;
        bridge
            .set_session_config_option(response.session_id.clone(), &config_id, model)
            .await?;
    }
    Ok(())
}

fn agent_test_spawn_error(error: StackError) -> StackError {
    let stage = match error {
        StackError::AgentSpawnFailed { .. } => "spawn/start",
        StackError::AgentInitializeFailed { .. } => "ACP initialize",
        _ => "spawn/start",
    };
    agent_test_error(stage, error)
}

fn agent_test_error(stage: &'static str, error: StackError) -> StackError {
    StackError::AgentTestFailed {
        stage: stage.to_owned(),
        reason: error.to_string(),
    }
}

fn stop_reason_label(reason: StopReason) -> String {
    match reason {
        StopReason::EndTurn => "end_turn".to_owned(),
        StopReason::MaxTokens => "max_tokens".to_owned(),
        StopReason::MaxTurnRequests => "max_turn_requests".to_owned(),
        StopReason::Refusal => "refusal".to_owned(),
        StopReason::Cancelled => "cancelled".to_owned(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn human_duration(duration: Duration) -> String {
    if duration.as_millis() < 1_000 {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{}s", duration.as_secs())
    }
}
