use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use agent_client_protocol::schema::v1::{
    ContentBlock, PromptRequest, SessionId, SessionUpdate, StopReason, TextContent,
};
use tokio::sync::Notify;

use crate::config::{self, Config};
use crate::error::{Result, StackError};
use crate::fs_util::home_dir;
use crate::runtime::agent::acp_bridge::{
    AcpBridge, AcpPermissionPolicy, AgentSessionConfigCategory, AgentSessionModeSelection,
    AgentSessionModelSelection, SessionEventSink, session_config_id_for_value,
    session_mode_selection_for_value, session_model_selection_for_value,
};
use crate::runtime::agent::model_discovery::{
    effort_value_is_explicit_without_discovery, model_applies_from_disk_only,
};
use crate::runtime::install::agent_registry::RegistryCatalog;

use super::install::{operator_registry_override, resolve_agent_env_for_cli};
use super::{
    AgentTestArgs, DEFAULT_AGENT_TEST_PROGRESS_TIMEOUT, DEFAULT_AGENT_TEST_PROMPT,
    DEFAULT_AGENT_TEST_TIMEOUT,
};
use crate::cli::core::{OutputFormat, print_json};

// CONSTANTS

/// Version of the `agent test --format json` document; any field change bumps it.
const AGENT_TEST_SCHEMA_VERSION: i64 = 1;

/// Phases, in run order; derived from the outcome code so text and JSON agree.
const PHASE_SPAWN: &str = "spawn";
const PHASE_INITIALIZE: &str = "initialize";
const PHASE_SESSION_NEW: &str = "session_new";
const PHASE_SESSION_CONFIG: &str = "session_config";
const PHASE_PROMPT: &str = "prompt";
const PHASE_FS_CHECK: &str = "fs_check";
const PHASE_CLEANUP: &str = "cleanup";
const PHASE_DONE: &str = "done";

/// Outcome codes: orchestrators classify a failed testflight from `code`, never `reason`.
const CODE_OK: &str = "ok";
const CODE_AGENT_SPAWN_FAILED: &str = "agent_spawn_failed";
const CODE_AGENT_INITIALIZE_FAILED: &str = "agent_initialize_failed";
const CODE_SESSION_CREATE_FAILED: &str = "session_create_failed";
const CODE_SESSION_CONFIG_FAILED: &str = "session_config_failed";
const CODE_PROMPT_FAILED: &str = "prompt_failed";
const CODE_PROMPT_TIMEOUT: &str = "prompt_timeout";
const CODE_PROGRESS_TIMEOUT: &str = "progress_timeout";
const CODE_UNEXPECTED_STOP_REASON: &str = "unexpected_stop_reason";
const CODE_FS_CHECK_MISSING: &str = "fs_check_missing";
const CODE_FS_CHECK_EMPTY: &str = "fs_check_empty";
const CODE_FS_CHECK_NOT_REGULAR_FILE: &str = "fs_check_not_regular_file";
const CODE_FS_CHECK_OUTSIDE_WORKSPACE: &str = "fs_check_outside_workspace";
const CODE_FS_CHECK_FAILED: &str = "fs_check_failed";
const CODE_CLEANUP_FAILED: &str = "cleanup_failed";
const CODE_CONFIG_INVALID: &str = "config_invalid";
const CODE_AGENT_UNSUPPORTED: &str = "agent_unsupported";

/// `fs_check.status` values; `skipped` covers "no declared artifact" and "never reached".
const FS_CHECK_OK: &str = "ok";
const FS_CHECK_SKIPPED: &str = "skipped";
const FS_CHECK_FAILED: &str = "failed";

/// `cleanup.session_delete` values.
const SESSION_DELETE_DELETED: &str = "deleted";
const SESSION_DELETE_CLEANUP_FAILED: &str = "cleanup_failed";
const SESSION_DELETE_UNSUPPORTED: &str = "unsupported";
const SESSION_DELETE_SKIPPED: &str = "skipped";

/// `cleanup.process` values; `terminated` means no agent child remains.
const PROCESS_TERMINATED: &str = "terminated";
const PROCESS_TERMINATE_FAILED: &str = "terminate_failed";

const STAGE_FS_CHECK: &str = "fs_check";

/// How long the disposable-session delete may take before falling through to termination.
const SESSION_DELETE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on the retained assistant-message tail.
const EVIDENCE_TEXT_TAIL_BYTES: usize = 2048;

/// Placeholder written over a resolved secret value in the assistant-text evidence.
const SECRET_REDACTION_PLACEHOLDER: &str = "[redacted]";

/// Shortest resolved env value treated as a secret; low-entropy settings are left alone.
const MIN_REDACTED_SECRET_LEN: usize = 6;

/// Shortest leading fragment of a secret redacted at the front-truncated tail's head.
const MIN_REDACTED_SECRET_FRAGMENT_LEN: usize = 8;

/// Some adapters answer the prompt before their final file writes are visible to a
/// stat from this process; re-poll briefly before declaring the artifact missing.
const FS_CHECK_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const FS_CHECK_SETTLE_INTERVAL: Duration = Duration::from_millis(100);

/// A typed `agent test` failure carrying the machine-readable outcome `code`.
#[derive(Debug)]
pub(super) struct AgentTestFailure {
    stage: &'static str,
    reason: String,
    code: &'static str,
}

impl AgentTestFailure {
    fn new(stage: &'static str, code: &'static str, reason: String) -> Self {
        Self {
            stage,
            reason,
            code,
        }
    }

    #[cfg(test)]
    pub(super) fn stage(&self) -> &str {
        self.stage
    }

    #[cfg(test)]
    pub(super) fn reason(&self) -> &str {
        &self.reason
    }

    #[cfg(test)]
    pub(super) fn code(&self) -> &'static str {
        self.code
    }
}

impl From<AgentTestFailure> for StackError {
    fn from(failure: AgentTestFailure) -> Self {
        StackError::AgentTestFailed {
            stage: failure.stage.to_owned(),
            reason: failure.reason,
            code: failure.code,
        }
    }
}

type TestResult<T> = std::result::Result<T, AgentTestFailure>;

fn phase_for_code(code: &str) -> &'static str {
    match code {
        CODE_AGENT_INITIALIZE_FAILED => PHASE_INITIALIZE,
        CODE_SESSION_CREATE_FAILED => PHASE_SESSION_NEW,
        CODE_SESSION_CONFIG_FAILED => PHASE_SESSION_CONFIG,
        CODE_PROMPT_FAILED
        | CODE_PROMPT_TIMEOUT
        | CODE_PROGRESS_TIMEOUT
        | CODE_UNEXPECTED_STOP_REASON => PHASE_PROMPT,
        CODE_FS_CHECK_MISSING
        | CODE_FS_CHECK_EMPTY
        | CODE_FS_CHECK_NOT_REGULAR_FILE
        | CODE_FS_CHECK_OUTSIDE_WORKSPACE
        | CODE_FS_CHECK_FAILED => PHASE_FS_CHECK,
        CODE_CLEANUP_FAILED => PHASE_CLEANUP,
        CODE_OK => PHASE_DONE,
        _ => PHASE_SPAWN,
    }
}

/// What the run's session updates said, totalled on every exit path.
#[derive(Default, Clone)]
struct AgentTestEvidence {
    /// Tail of the concatenated `agent_message_chunk` text, bounded by
    /// [`EVIDENCE_TEXT_TAIL_BYTES`].
    final_assistant_text: String,
    text_truncated: bool,
    message_chunks: usize,
    thought_chunks: usize,
    tool_calls: usize,
    tool_call_updates: usize,
}

impl AgentTestEvidence {
    fn record(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                self.message_chunks += 1;
                if let ContentBlock::Text(text) = &chunk.content {
                    self.push_text(&text.text);
                }
            }
            SessionUpdate::AgentThoughtChunk(_) => self.thought_chunks += 1,
            SessionUpdate::ToolCall(_) => self.tool_calls += 1,
            SessionUpdate::ToolCallUpdate(_) => self.tool_call_updates += 1,
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        self.final_assistant_text.push_str(text);
        if self.final_assistant_text.len() <= EVIDENCE_TEXT_TAIL_BYTES {
            return;
        }
        self.text_truncated = true;
        let excess = self.final_assistant_text.len() - EVIDENCE_TEXT_TAIL_BYTES;
        let mut cut = excess;
        while !self.final_assistant_text.is_char_boundary(cut) {
            cut += 1;
        }
        self.final_assistant_text.drain(..cut);
    }
}

/// Redact resolved secret values from the assistant-text tail before it is emitted as
/// JSON: the test agent runs with credentials in its env and auto-approves tools, so
/// the diagnostic tail must not become an exfiltration channel.
fn redact_secret_values(text: &mut String, secret_values: &[String], text_truncated: bool) {
    for value in secret_values {
        if value.len() < MIN_REDACTED_SECRET_LEN {
            continue;
        }
        if text.contains(value.as_str()) {
            *text = text.replace(value.as_str(), SECRET_REDACTION_PLACEHOLDER);
        }
        if text_truncated {
            redact_leading_secret_fragment(text, value);
        }
    }
}

/// Redact the longest suffix of `value` that `text` begins with — the tail is
/// truncated from the front, so a straddling secret leaves only a suffix fragment.
fn redact_leading_secret_fragment(text: &mut String, value: &str) {
    let max = value.len().min(text.len());
    for len in (MIN_REDACTED_SECRET_FRAGMENT_LEN..=max).rev() {
        let suffix_start = value.len() - len;
        if !value.is_char_boundary(suffix_start) {
            continue;
        }
        let suffix = &value[suffix_start..];
        if text.starts_with(suffix) {
            text.replace_range(..suffix.len(), SECRET_REDACTION_PLACEHOLDER);
            return;
        }
    }
}

struct AgentTestSessionEventSink {
    updates: AtomicUsize,
    notify: Notify,
    evidence: std::sync::Mutex<AgentTestEvidence>,
}

impl AgentTestSessionEventSink {
    fn new() -> Self {
        Self {
            updates: AtomicUsize::new(0),
            notify: Notify::new(),
            evidence: std::sync::Mutex::new(AgentTestEvidence::default()),
        }
    }

    fn update_count(&self) -> usize {
        self.updates.load(Ordering::SeqCst)
    }

    fn evidence(&self) -> AgentTestEvidence {
        self.evidence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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
    fn capture_session_update<'a>(
        &'a self,
        _agent_session_id: &'a str,
        update: &'a SessionUpdate,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            self.evidence
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record(update);
            true
        })
    }

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

#[derive(Clone, Copy)]
struct CleanupOutcome {
    session_delete: &'static str,
    process: &'static str,
}

impl CleanupOutcome {
    /// The pre-spawn state: nothing was created, and no agent child remains.
    fn nothing_to_clean() -> Self {
        Self {
            session_delete: SESSION_DELETE_SKIPPED,
            process: PROCESS_TERMINATED,
        }
    }
}

/// What one `agent test` run observed. The harness fields report codes only: no reason
/// strings, prompt text, or paths. `evidence` is the deliberate exception, carrying
/// arbitrary agent output (final assistant text, update-kind counts) scrubbed only of the
/// secret values this process injected into the agent's env (see [`redact_secret_values`]),
/// so a credentialed auto-approving run cannot echo those back. That scrub is the bound of
/// the guarantee: credentials the agent reads from its own on-disk config, or prompt and
/// workspace-file content it echoes, are agent-authored, unknowable here, and retained
/// deliberately.
struct AgentTestOutcome {
    ok: bool,
    code: &'static str,
    elapsed_ms: u64,
    agent: String,
    prompt_source: AgentTestPromptSource,
    session_id: Option<String>,
    stop_reason: Option<StopReason>,
    updates: usize,
    evidence: AgentTestEvidence,
    fs_check_status: &'static str,
    fs_check_bytes: Option<u64>,
    fs_check_path: Option<PathBuf>,
    cleanup: CleanupOutcome,
}

impl AgentTestOutcome {
    fn starting(agent: String, prompt_provided: bool) -> Self {
        Self {
            ok: false,
            code: CODE_OK,
            elapsed_ms: 0,
            agent,
            prompt_source: if prompt_provided {
                AgentTestPromptSource::CliFlag
            } else {
                AgentTestPromptSource::Default
            },
            session_id: None,
            stop_reason: None,
            updates: 0,
            evidence: AgentTestEvidence::default(),
            fs_check_status: FS_CHECK_SKIPPED,
            fs_check_bytes: None,
            fs_check_path: None,
            cleanup: CleanupOutcome::nothing_to_clean(),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema_version": AGENT_TEST_SCHEMA_VERSION,
            "ok": self.ok,
            "phase": phase_for_code(self.code),
            "code": self.code,
            "elapsed_ms": self.elapsed_ms,
            "agent": self.agent,
            "prompt_source": self.prompt_source.label(),
            "stop_reason": self.stop_reason.map(stop_reason_label),
            "updates": self.updates,
            "evidence": {
                "final_assistant_text": self.evidence.final_assistant_text,
                "text_truncated": self.evidence.text_truncated,
                "message_chunks": self.evidence.message_chunks,
                "thought_chunks": self.evidence.thought_chunks,
                "tool_calls": self.evidence.tool_calls,
                "tool_call_updates": self.evidence.tool_call_updates,
            },
            "fs_check": {
                "status": self.fs_check_status,
                "bytes": self.fs_check_bytes,
            },
            "cleanup": {
                "session_delete": self.cleanup.session_delete,
                "process": self.cleanup.process,
            },
        })
    }
}

struct AgentTestRun {
    outcome: AgentTestOutcome,
    error: Option<StackError>,
}

/// Run a real-prompt testflight at the tail of `acps init`.
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
    let run = run_agent_test_with(home, config, registry, args);
    if print_summary && run.error.is_none() {
        print_agent_test_summary(&run.outcome);
    }
    match run.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(super) fn run_agent_test(args: AgentTestArgs, format: OutputFormat) -> Result<()> {
    let home = home_dir()?;
    let config = Config::load_from_default_path()?;
    let registry = RegistryCatalog::load_with_override(&operator_registry_override(&home))?;
    let run = run_agent_test_with(&home, &config, &registry, args);
    if format.is_json() {
        print_json(&run.outcome.to_json())?;
    } else if run.error.is_none() {
        print_agent_test_summary(&run.outcome);
    }
    match run.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn print_agent_test_summary(outcome: &AgentTestOutcome) {
    println!("agent test: ok");
    println!("agent: {}", outcome.agent);
    println!("prompt: {}", outcome.prompt_source.label());
    if let Some(session_id) = outcome.session_id.as_deref() {
        println!("session_id: {session_id}");
    }
    if let Some(stop_reason) = outcome.stop_reason {
        println!("stop_reason: {}", stop_reason_label(stop_reason));
    }
    println!("updates: {}", outcome.updates);
    println!(
        "tool_calls: {} ({} updates)",
        outcome.evidence.tool_calls, outcome.evidence.tool_call_updates
    );
    if let (Some(bytes), Some(path)) = (outcome.fs_check_bytes, outcome.fs_check_path.as_ref()) {
        println!("fs_check: ok ({bytes} bytes at {})", path.display());
    }
}

fn run_agent_test_with(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    args: AgentTestArgs,
) -> AgentTestRun {
    // Wall clock, not `Instant`: a VM's monotonic clock stalls across a host suspend.
    let started = SystemTime::now();
    let mut outcome = AgentTestOutcome::starting(config.agent.id.clone(), args.prompt.is_some());
    let failure = execute_agent_test(home, config, registry, args, &mut outcome).err();
    outcome.elapsed_ms = SystemTime::now()
        .duration_since(started)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    match failure {
        None => {
            outcome.ok = true;
            outcome.code = CODE_OK;
            AgentTestRun {
                outcome,
                error: None,
            }
        }
        Some(failure) => {
            outcome.ok = false;
            outcome.code = failure.code;
            AgentTestRun {
                outcome,
                error: Some(failure.into()),
            }
        }
    }
}

fn execute_agent_test(
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    args: AgentTestArgs,
    outcome: &mut AgentTestOutcome,
) -> TestResult<()> {
    let entry = registry
        .lookup_required(&config.agent.id)
        .and_then(|entry| entry.ensure_supported().map(|()| entry))
        .map_err(|error| {
            AgentTestFailure::new(
                "spawn/start",
                CODE_AGENT_UNSUPPORTED,
                format!("agent `{}` is not testable: {error}", config.agent.id),
            )
        })?;

    let prompt_source = if args.prompt.is_some() {
        AgentTestPromptSource::CliFlag
    } else if entry.testflight_prompt.is_some() {
        AgentTestPromptSource::Registry
    } else {
        AgentTestPromptSource::Default
    };
    outcome.prompt_source = prompt_source;
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
    let env = resolve_agent_env_for_cli(home, config).map_err(|error| {
        AgentTestFailure::new(
            "spawn/start",
            CODE_CONFIG_INVALID,
            format!("resolve agent launch environment failed: {error}"),
        )
    })?;
    // Snapshot the resolved secrets before `env` moves: the evidence tail is scrubbed
    // of them below so a run that echoes a credential cannot leak it into the JSON.
    let secret_values: Vec<String> = env.values().cloned().collect();
    let cwd = config
        .agent
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.workspace.root));
    let agent = config.agent.clone();

    // The agent resolves relative paths against its session cwd, so the artifact is
    // prepared and verified there; the workspace root stays the containment boundary.
    let artifact_base = cwd.clone();
    if let Some(rel) = expect_fs.as_deref() {
        prepare_testflight_expect_fs(&artifact_base, &workspace_root, rel)?;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| {
            AgentTestFailure::new(
                "spawn/start",
                CODE_CONFIG_INVALID,
                format!("build agent test runtime failed: {source}"),
            )
        })?;
    let sandbox = config.workspace.sandbox.clone();
    let shell = config.workspace.default_shell.clone();
    let network_provider = crate::extensions::resolve_network_provider(config);
    let report = runtime.block_on(async move {
        run_agent_test_inner(
            home,
            agent,
            env,
            cwd,
            prompt,
            timeout,
            progress_timeout,
            sandbox,
            shell,
            network_provider,
        )
        .await
    });

    // Cleanup is observed on every exit path, so record it before propagating failure.
    outcome.cleanup = report.cleanup;
    outcome.session_id = report.session_id;
    outcome.stop_reason = report.stop_reason;
    outcome.updates = report.updates;
    outcome.evidence = report.evidence;
    redact_secret_values(
        &mut outcome.evidence.final_assistant_text,
        &secret_values,
        outcome.evidence.text_truncated,
    );
    if let Some(failure) = report.failure {
        return Err(failure);
    }

    if let Some(rel) = expect_fs.as_deref() {
        match verify_testflight_expect_fs_settled(&artifact_base, &workspace_root, rel) {
            Ok(fs_outcome) => {
                outcome.fs_check_status = FS_CHECK_OK;
                outcome.fs_check_bytes = Some(fs_outcome.bytes);
                outcome.fs_check_path = Some(fs_outcome.path);
            }
            Err(failure) => {
                outcome.fs_check_status = FS_CHECK_FAILED;
                return Err(failure);
            }
        }
    }
    Ok(())
}

/// [`verify_testflight_expect_fs`] behind the [`FS_CHECK_SETTLE_TIMEOUT`] re-poll window.
fn verify_testflight_expect_fs_settled(
    artifact_base: &Path,
    workspace_root: &Path,
    relative: &str,
) -> TestResult<TestflightFsOutcome> {
    let deadline = std::time::Instant::now() + FS_CHECK_SETTLE_TIMEOUT;
    loop {
        match verify_testflight_expect_fs(artifact_base, workspace_root, relative) {
            Ok(outcome) => return Ok(outcome),
            Err(failure)
                if matches!(failure.code, CODE_FS_CHECK_MISSING | CODE_FS_CHECK_EMPTY)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(FS_CHECK_SETTLE_INTERVAL);
            }
            Err(failure) => return Err(failure),
        }
    }
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

/// Clear any stale testflight artifact before the prompt runs. Paths resolve against
/// `artifact_base` (the agent's session cwd) while `workspace_root` stays the
/// containment boundary.
pub(super) fn prepare_testflight_expect_fs(
    artifact_base: &Path,
    workspace_root: &Path,
    relative: &str,
) -> TestResult<()> {
    let path = testflight_expect_fs_path(artifact_base, relative)?;
    ensure_testflight_parent_within_workspace(workspace_root, &path)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::remove_file(&path).map_err(|source| {
                AgentTestFailure::new(
                    STAGE_FS_CHECK,
                    CODE_FS_CHECK_FAILED,
                    format!(
                        "remove stale testflight artifact `{}` failed: {source}",
                        path.display()
                    ),
                )
            })?;
            Ok(())
        }
        Ok(metadata) => Err(AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_NOT_REGULAR_FILE,
            format!(
                "pre-existing testflight artifact `{}` is {}; remove it before running testflight",
                path.display(),
                if metadata.file_type().is_symlink() {
                    "a symlink"
                } else {
                    "not a regular file"
                }
            ),
        )),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_FAILED,
            format!(
                "stat pre-existing testflight artifact `{}` failed: {source}",
                path.display()
            ),
        )),
    }
}

pub(super) fn verify_testflight_expect_fs(
    artifact_base: &Path,
    workspace_root: &Path,
    relative: &str,
) -> TestResult<TestflightFsOutcome> {
    let path = testflight_expect_fs_path(artifact_base, relative)?;
    let workspace = workspace_root.canonicalize().map_err(|source| {
        AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_FAILED,
            format!(
                "canonicalize workspace root `{}` failed: {source}",
                workspace_root.display()
            ),
        )
    })?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|source| {
        AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_MISSING,
            format!(
                "expected agent to create `{}` (workspace-relative `{}`) but stat failed: {source}",
                path.display(),
                relative
            ),
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_NOT_REGULAR_FILE,
            format!(
                "expected agent to create regular file `{}`, but it is a symlink",
                path.display()
            ),
        ));
    }
    if !metadata.is_file() {
        return Err(AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_NOT_REGULAR_FILE,
            format!(
                "expected agent to create regular file `{}`, but it is not a regular file",
                path.display()
            ),
        ));
    }
    let canonical_path = path.canonicalize().map_err(|source| {
        AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_FAILED,
            format!(
                "canonicalize testflight artifact `{}` failed: {source}",
                path.display()
            ),
        )
    })?;
    if !canonical_path.starts_with(&workspace) {
        return Err(AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_OUTSIDE_WORKSPACE,
            format!(
                "testflight artifact `{}` resolved outside workspace `{}`",
                canonical_path.display(),
                workspace.display()
            ),
        ));
    }
    if metadata.len() == 0 {
        return Err(AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_EMPTY,
            format!(
                "agent created `{}` but the file is empty; treating as no tool action",
                path.display()
            ),
        ));
    }
    Ok(TestflightFsOutcome {
        path,
        bytes: metadata.len(),
    })
}

fn testflight_expect_fs_path(workspace_root: &Path, relative: &str) -> TestResult<PathBuf> {
    if Path::new(relative).is_absolute() || relative.split('/').any(|seg| seg == "..") {
        return Err(AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_CONFIG_INVALID,
            format!(
                "testflight_expect_fs `{relative}` must be a workspace-relative path with no `..` segments"
            ),
        ));
    }
    Ok(workspace_root.join(relative))
}

fn ensure_testflight_parent_within_workspace(workspace_root: &Path, path: &Path) -> TestResult<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let workspace = workspace_root.canonicalize().map_err(|source| {
        AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_FAILED,
            format!(
                "canonicalize workspace root `{}` failed: {source}",
                workspace_root.display()
            ),
        )
    })?;
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AgentTestFailure::new(
                STAGE_FS_CHECK,
                CODE_FS_CHECK_FAILED,
                format!("canonicalize `{}` failed: {source}", parent.display()),
            ));
        }
    };
    if parent.starts_with(&workspace) {
        Ok(())
    } else {
        Err(AgentTestFailure::new(
            STAGE_FS_CHECK,
            CODE_FS_CHECK_OUTSIDE_WORKSPACE,
            format!(
                "testflight artifact parent `{}` resolved outside workspace `{}`",
                parent.display(),
                workspace.display()
            ),
        ))
    }
}

fn parse_agent_test_duration(field: &'static str, value: &str) -> TestResult<Duration> {
    let duration = config::parse_duration_string(value).filter(|parsed| !parsed.is_zero());
    duration.ok_or_else(|| {
        AgentTestFailure::new(
            "spawn/start",
            CODE_CONFIG_INVALID,
            format!("`{field}` must be a non-zero duration"),
        )
    })
}

/// What the ACP side of the run produced; total, so the failure travels as a field.
struct AgentTestInnerReport {
    session_id: Option<String>,
    stop_reason: Option<StopReason>,
    updates: usize,
    evidence: AgentTestEvidence,
    cleanup: CleanupOutcome,
    failure: Option<AgentTestFailure>,
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_test_inner(
    home: &Path,
    agent: crate::config::AgentConfig,
    env: HashMap<String, String>,
    cwd: PathBuf,
    prompt: String,
    prompt_timeout: Duration,
    progress_timeout: Duration,
    sandbox: crate::config::SandboxConfig,
    shell: String,
    network_provider: Option<crate::extensions::NetworkProviderExtension>,
) -> AgentTestInnerReport {
    let sink = Arc::new(AgentTestSessionEventSink::new());
    let bridge = match AcpBridge::spawn(
        home,
        &agent,
        env,
        cwd.clone(),
        sink.clone(),
        AcpPermissionPolicy::AutoApprove,
        &sandbox,
        &shell,
        network_provider.as_ref(),
        None,
    )
    .await
    {
        Ok(bridge) => bridge,
        Err(error) => {
            // `AcpBridge::spawn` kills its own child on every failure path.
            return AgentTestInnerReport {
                session_id: None,
                stop_reason: None,
                updates: 0,
                evidence: AgentTestEvidence::default(),
                cleanup: CleanupOutcome::nothing_to_clean(),
                failure: Some(agent_test_spawn_error(error)),
            };
        }
    };

    let mut created_session: Option<SessionId> = None;
    let mut stop_reason = None;
    let result = {
        let created_session: &mut Option<SessionId> = &mut created_session;
        let stop_reason = &mut stop_reason;
        async {
            let session = bridge.new_session(cwd, Vec::new()).await.map_err(|err| {
                agent_test_error("session creation", CODE_SESSION_CREATE_FAILED, err)
            })?;
            *created_session = Some(session.session_id.clone());
            apply_agent_test_session_config(&bridge, &agent, &session)
                .await
                .map_err(|err| {
                    agent_test_error("session creation", CODE_SESSION_CONFIG_FAILED, err)
                })?;
            let request = PromptRequest::new(
                session.session_id.clone(),
                vec![ContentBlock::Text(TextContent::new(prompt))],
            );
            let reason = run_agent_test_prompt(
                &bridge,
                request,
                sink.clone(),
                prompt_timeout,
                progress_timeout,
            )
            .await?;
            *stop_reason = Some(reason);
            if reason != StopReason::EndTurn {
                return Err(AgentTestFailure::new(
                    "prompt completion",
                    CODE_UNEXPECTED_STOP_REASON,
                    format!(
                        "expected stop_reason end_turn, got {}",
                        stop_reason_label(reason)
                    ),
                ));
            }
            Ok(())
        }
        .await
    };

    // Delete the disposable session before shutdown: once the bridge is down the agent
    // can no longer be asked to forget it.
    let session_id = created_session
        .as_ref()
        .map(|session_id| session_id.to_string());
    let session_delete = match created_session {
        None => SESSION_DELETE_SKIPPED,
        Some(_) if !bridge.capabilities().supports_delete_session() => SESSION_DELETE_UNSUPPORTED,
        // Bounded: an agent wedged mid-prompt never answers `session/delete`, and
        // `shutdown()` below still reclaims the process.
        Some(session_id) => {
            match tokio::time::timeout(SESSION_DELETE_TIMEOUT, bridge.delete_session(session_id))
                .await
            {
                Ok(Ok(())) => SESSION_DELETE_DELETED,
                Ok(Err(error)) => {
                    tracing::warn!(
                        error = %error,
                        "agent test could not delete its disposable session"
                    );
                    SESSION_DELETE_CLEANUP_FAILED
                }
                Err(_) => {
                    tracing::warn!(
                        "agent test timed out deleting its disposable session; \
                         terminating the agent process anyway"
                    );
                    SESSION_DELETE_CLEANUP_FAILED
                }
            }
        }
    };
    let process = match bridge.shutdown().await {
        Ok(_) => PROCESS_TERMINATED,
        Err(error) => {
            tracing::warn!(error = %error, "agent test could not terminate the agent process");
            PROCESS_TERMINATE_FAILED
        }
    };
    let cleanup = CleanupOutcome {
        session_delete,
        process,
    };

    // A failed session delete does not flip the verdict; a leaked agent child does.
    let failure = match (result, process) {
        (Err(failure), _) => Some(failure),
        (Ok(()), PROCESS_TERMINATE_FAILED) => Some(AgentTestFailure::new(
            "shutdown",
            CODE_CLEANUP_FAILED,
            "agent process did not terminate after the test".to_owned(),
        )),
        (Ok(()), _) => None,
    };

    AgentTestInnerReport {
        session_id,
        stop_reason,
        updates: sink.update_count(),
        evidence: sink.evidence(),
        cleanup,
        failure,
    }
}

async fn run_agent_test_prompt(
    bridge: &AcpBridge,
    request: PromptRequest,
    sink: Arc<AgentTestSessionEventSink>,
    prompt_timeout: Duration,
    progress_timeout: Duration,
) -> TestResult<StopReason> {
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
                        .map_err(|err| agent_test_error("prompt completion", CODE_PROMPT_FAILED, err));
                }
                _ = sink.wait_for_update_after(observed_updates) => {
                    observed_updates = sink.update_count();
                }
                _ = &mut progress_timer => {
                    return Err(AgentTestFailure::new(
                        "prompt/progress timeout",
                        CODE_PROGRESS_TIMEOUT,
                        format!(
                            "no new session/update or terminal prompt response within {}",
                            human_duration(progress_timeout)
                        ),
                    ));
                }
            }
        }
    };

    tokio::time::timeout(prompt_timeout, prompt_call)
        .await
        .map_err(|_| {
            AgentTestFailure::new(
                "prompt/progress timeout",
                CODE_PROMPT_TIMEOUT,
                format!(
                    "prompt did not complete within {}",
                    human_duration(prompt_timeout)
                ),
            )
        })?
}

async fn apply_agent_test_session_config(
    bridge: &AcpBridge,
    agent: &crate::config::AgentConfig,
    response: &agent_client_protocol::schema::v1::NewSessionResponse,
) -> Result<()> {
    if let Some(mode) = agent.mode.as_deref() {
        match session_mode_selection_for_value(response, mode)? {
            AgentSessionModeSelection::ConfigOption { config_id } => {
                bridge
                    .set_session_config_option(response.session_id.clone(), &config_id, mode)
                    .await?;
            }
            AgentSessionModeSelection::NativeMode { mode_id } => {
                bridge
                    .set_session_mode(response.session_id.clone(), &mode_id)
                    .await?;
            }
        }
    }
    if let Some(model) = agent.model.as_deref().or_else(|| {
        agent
            .provider
            .as_ref()
            .and_then(|provider| provider.model.as_deref())
    }) {
        if model_applies_from_disk_only(agent) {
            // Same skip as the supervisor: the harness reads this pin from its
            // on-disk config, so the advertised-list match can only fail spuriously.
            tracing::debug!(
                model,
                "model provisioned on disk; skipping session/set_config_option"
            );
        } else {
            let AgentSessionModelSelection::ConfigOption { config_id } =
                session_model_selection_for_value(response, model)?;
            bridge
                .set_session_config_option(response.session_id.clone(), &config_id, model)
                .await?;
        }
    }
    if let Some(effort) = agent.effort.as_deref() {
        if effort_value_is_explicit_without_discovery(agent) {
            // Same skip as the supervisor: the pin lives in the harness's on-disk config
            // and the adapter advertises no effort option for this model, so a set can
            // only fail spuriously.
            tracing::debug!(
                effort,
                "effort provisioned on disk; skipping session/set_config_option"
            );
        } else {
            let config_id = session_config_id_for_value(
                response.config_options.as_deref(),
                AgentSessionConfigCategory::Effort,
                effort,
            )?;
            bridge
                .set_session_config_option(response.session_id.clone(), &config_id, effort)
                .await?;
        }
    }
    Ok(())
}

fn agent_test_spawn_error(error: StackError) -> AgentTestFailure {
    let (stage, code) = match error {
        StackError::AgentInitializeFailed { .. } => {
            ("ACP initialize", CODE_AGENT_INITIALIZE_FAILED)
        }
        _ => ("spawn/start", CODE_AGENT_SPAWN_FAILED),
    };
    agent_test_error(stage, code, error)
}

fn agent_test_error(
    stage: &'static str,
    code: &'static str,
    error: StackError,
) -> AgentTestFailure {
    AgentTestFailure::new(stage, code, error.to_string())
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

#[cfg(test)]
mod evidence_tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentChunk, ToolCall};

    fn message_chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        ))))
    }

    #[test]
    fn evidence_counts_update_kinds_and_keeps_message_text() {
        let mut evidence = AgentTestEvidence::default();
        evidence.record(&message_chunk("I created "));
        evidence.record(&message_chunk("the file."));
        evidence.record(&SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("thinking")),
        )));
        evidence.record(&SessionUpdate::ToolCall(ToolCall::new(
            "tc_1".to_owned(),
            "write file".to_owned(),
        )));

        assert_eq!(evidence.final_assistant_text, "I created the file.");
        assert!(!evidence.text_truncated);
        assert_eq!(evidence.message_chunks, 2);
        assert_eq!(evidence.thought_chunks, 1);
        assert_eq!(evidence.tool_calls, 1);
        assert_eq!(evidence.tool_call_updates, 0);
    }

    #[test]
    fn evidence_text_keeps_a_bounded_tail_on_char_boundaries() {
        let mut evidence = AgentTestEvidence::default();
        let filler = "é".repeat(EVIDENCE_TEXT_TAIL_BYTES);
        evidence.record(&message_chunk(&filler));
        evidence.record(&message_chunk("final words"));

        assert!(evidence.text_truncated);
        assert!(evidence.final_assistant_text.len() <= EVIDENCE_TEXT_TAIL_BYTES);
        assert!(evidence.final_assistant_text.ends_with("final words"));
        assert!(evidence.final_assistant_text.is_char_boundary(0));
    }

    #[test]
    fn redaction_scrubs_full_and_straddling_secret_values() {
        let secrets = vec![
            "sk-supersecretkey-ABCDEF".to_owned(),
            "on".to_owned(),
            "https://api.example.test/v1".to_owned(),
        ];

        let mut text = "I wrote the key sk-supersecretkey-ABCDEF to the file.".to_owned();
        redact_secret_values(&mut text, &secrets, false);
        assert!(!text.contains("sk-supersecretkey-ABCDEF"));
        assert!(text.contains(SECRET_REDACTION_PLACEHOLDER));

        let mut short = "mode is on now".to_owned();
        redact_secret_values(&mut short, &secrets, true);
        assert_eq!(short, "mode is on now");

        let mut straddled = "retkey-ABCDEF was the tail".to_owned();
        redact_secret_values(&mut straddled, &secrets, true);
        assert!(straddled.starts_with(SECRET_REDACTION_PLACEHOLDER));
        assert!(straddled.ends_with(" was the tail"));
        assert!(!straddled.contains("retkey-ABCDEF"));

        let mut untruncated = "retkey-ABCDEF was the tail".to_owned();
        redact_secret_values(&mut untruncated, &secrets, false);
        assert_eq!(untruncated, "retkey-ABCDEF was the tail");

        let mut clean = "created report.txt with the requested summary".to_owned();
        redact_secret_values(&mut clean, &secrets, true);
        assert_eq!(clean, "created report.txt with the requested summary");
    }
}
