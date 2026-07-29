//! Prompt/session input parsing and path resolution helpers.
//!
//! These are the pure, mostly-synchronous conversions the supervisor performs
//! at request boundaries: client JSON into typed ACP values, settled prompt
//! results into the persisted failure taxonomy, and raw cwd strings into paths
//! validated against `workspace.root`.

use super::*;

pub(super) enum Outcome {
    Settled(Result<PromptResponse>),
    Cancelled,
}

/// Owned fields the spawned prompt task hands to the state store on settle.
/// Built before the await on the state mutex so we never hold the lock while
/// constructing JSON payloads.
pub(super) struct TerminalOutcome {
    pub(super) status: PromptStatus,
    pub(super) stop_reason: Option<String>,
    pub(super) error_code: Option<String>,
    pub(super) error_message: Option<String>,
    pub(super) failure_class: Option<&'static str>,
    pub(super) failure_detail_json: Option<String>,
    pub(super) session_event: Option<TerminalSessionEvent>,
}

/// Companion session-scoped event emitted alongside the terminal status write.
/// Cancellation produces no event because cancellation is not a failure.
pub(super) struct TerminalSessionEvent {
    pub(super) level: &'static str,
    pub(super) kind: &'static str,
    pub(super) message: &'static str,
    pub(super) payload_json: String,
}

/// Build the persisted taxonomy + session event for a settled prompt task.
/// The `prompt_id` is threaded through the spawned task and embedded into the
/// session-event payload so dashboards can join on it; the row itself already
/// carries the prompt_id, but the event lives in a separate index.
pub(super) fn build_terminal_outcome_with_prompt_id(
    outcome: Outcome,
    prompt_id_for_event: Option<&str>,
) -> TerminalOutcome {
    match outcome {
        Outcome::Settled(Ok(response)) => {
            let stop_reason = response.stop_reason;
            let stop_str = stop_reason_str(stop_reason);
            let status = if stop_reason == StopReason::Cancelled {
                PromptStatus::Cancelled
            } else {
                PromptStatus::Completed
            };
            TerminalOutcome {
                status,
                stop_reason: Some(stop_str),
                error_code: None,
                error_message: None,
                failure_class: None,
                failure_detail_json: None,
                session_event: None,
            }
        }
        Outcome::Settled(Err(err)) => {
            let code = err.error_code().to_owned();
            let public = err.public_message();
            match &err {
                StackError::InferenceRequestFailed {
                    status_code,
                    reason_category,
                } => {
                    let failure_class = if (400..500).contains(status_code) {
                        FailureClass::Inference4xx
                    } else {
                        FailureClass::Inference5xx
                    };
                    let detail = json!({
                        "status_code": status_code,
                        "reason_category": reason_category,
                    })
                    .to_string();
                    let payload = json!({
                        "prompt_id": prompt_id_for_event,
                        "status_code": status_code,
                        "reason_category": reason_category,
                    })
                    .to_string();
                    TerminalOutcome {
                        status: PromptStatus::Errored,
                        stop_reason: None,
                        error_code: Some(code),
                        error_message: Some(public),
                        failure_class: Some(failure_class.as_str()),
                        failure_detail_json: Some(detail),
                        session_event: Some(TerminalSessionEvent {
                            level: "warn",
                            kind: EVENT_KIND_PROMPT_INFERENCE_FAILED,
                            message: "inference endpoint failure",
                            payload_json: payload,
                        }),
                    }
                }
                err => {
                    let Some(failure_class) = failure_class_for_prompt_error(err) else {
                        // Other terminal errors: persist with no failure_class
                        // (the taxonomy intentionally has gaps until callers add
                        // the right entry) but still emit the generic errored
                        // event so observers see the transition.
                        let payload = json!({
                            "prompt_id": prompt_id_for_event,
                            "error_code": code,
                        })
                        .to_string();
                        return TerminalOutcome {
                            status: PromptStatus::Errored,
                            stop_reason: None,
                            error_code: Some(code),
                            error_message: Some(public),
                            failure_class: None,
                            failure_detail_json: None,
                            session_event: Some(TerminalSessionEvent {
                                level: "error",
                                kind: EVENT_KIND_PROMPT_ERRORED,
                                message: "prompt failed",
                                payload_json: payload,
                            }),
                        };
                    };
                    let payload = json!({
                        "prompt_id": prompt_id_for_event,
                        "error_code": code,
                    })
                    .to_string();
                    TerminalOutcome {
                        status: PromptStatus::Errored,
                        stop_reason: None,
                        error_code: Some(code),
                        error_message: Some(public),
                        failure_class: Some(failure_class.as_str()),
                        failure_detail_json: None,
                        session_event: Some(TerminalSessionEvent {
                            level: "error",
                            kind: EVENT_KIND_PROMPT_ERRORED,
                            message: "prompt failed",
                            payload_json: payload,
                        }),
                    }
                }
            }
        }
        Outcome::Cancelled => TerminalOutcome {
            status: PromptStatus::Cancelled,
            stop_reason: Some("cancelled".to_owned()),
            error_code: None,
            error_message: None,
            failure_class: None,
            failure_detail_json: None,
            session_event: None,
        },
    }
}

fn failure_class_for_prompt_error(err: &StackError) -> Option<FailureClass> {
    match err {
        StackError::AgentRequestFailed { .. } => Some(FailureClass::AgentRequest),
        StackError::State(_) => Some(FailureClass::Sqlite),
        StackError::AgentSpawnFailed { .. }
        | StackError::AgentAlreadyRunning
        | StackError::AgentNotRunning
        | StackError::AgentInitializeFailed { .. }
        | StackError::AgentNotInitialized
        | StackError::AgentUnsupportedCapability { .. }
        | StackError::AgentApiRequest { .. }
        | StackError::AgentApiStatus { .. }
        | StackError::AgentTestFailed { .. } => Some(FailureClass::AgentProcess),
        StackError::ServeIo { .. } | StackError::ServeBind { .. } => Some(FailureClass::Daemon),
        _ => None,
    }
}

fn stop_reason_str(reason: StopReason) -> String {
    match reason {
        StopReason::EndTurn => "end_turn".to_owned(),
        StopReason::MaxTokens => "max_tokens".to_owned(),
        StopReason::MaxTurnRequests => "max_turn_requests".to_owned(),
        StopReason::Refusal => "refusal".to_owned(),
        StopReason::Cancelled => "cancelled".to_owned(),
        // StopReason is #[non_exhaustive]; future SDK additions surface as
        // the wire string verbatim until we add a typed mapping for them.
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Convert client-supplied prompt JSON into the typed `ContentBlock` vec the
/// ACP SDK requires. The accepted shape is `[{ "type": "text", "text": "..." }]`
/// (camelCase) or a bare string for convenience. Other ACP content variants
/// (resource, resource_link, image, audio) round-trip through `serde_json::from_value`.
pub fn parse_prompt_blocks(prompt: &Value) -> Result<Vec<ContentBlock>> {
    let blocks = match prompt {
        Value::String(text) => vec![ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new(text.clone()),
        )],
        Value::Array(items) => {
            if items.is_empty() {
                return Err(StackError::PromptBodyEmpty);
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let block: ContentBlock = serde_json::from_value(item.clone())
                    .map_err(|err| StackError::PromptBodyInvalid(err.to_string()))?;
                out.push(block);
            }
            out
        }
        Value::Null => return Err(StackError::PromptBodyEmpty),
        other => {
            return Err(StackError::PromptBodyInvalid(format!(
                "prompt must be a string or array, got {}",
                value_kind(other)
            )));
        }
    };
    Ok(blocks)
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Parse the optional `mcp_servers` field of a session create/load body into
/// the SDK's `Vec<McpServer>`.
pub fn parse_mcp_servers(value: Option<&Value>) -> Result<Vec<McpServer>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    if value.is_null() {
        return Ok(vec![]);
    }
    serde_json::from_value(value.clone())
        .map_err(|err| StackError::PromptBodyInvalid(format!("mcp_servers invalid: {err}")))
}

/// Hash `[agent].command` and compare against `expected_sha256`. Returns
/// `AgentSha256Mismatch` on mismatch and `AgentSpawnFailed` if the file
/// cannot be read. Path resolution mirrors what `tokio::process::Command`
/// will do at spawn time: bare names look up `$PATH`, relative paths with
/// a `/` resolve against `cwd`, absolute paths are used as-is.
pub(super) fn verify_agent_binary_sha256(
    command: &str,
    cwd: &std::path::Path,
    expected: &str,
) -> Result<()> {
    let path =
        resolve_command_path(command, cwd).ok_or_else(|| StackError::AgentInitializeFailed {
            reason: format!("agent command `{command}` not found on PATH"),
        })?;
    let bytes = std::fs::read(&path).map_err(|source| StackError::AgentSpawnFailed { source })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        return Err(StackError::AgentSha256Mismatch {
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}

/// Resolve `[agent].env` names against the secret store. Returns an empty
/// map when the list is empty so the secret store is never opened by
/// no-secret agents (relevant for tests and stripped-down deployments).
pub fn resolve_agent_env(
    agent: &AgentConfig,
    secrets: &SecretStore,
) -> Result<HashMap<String, String>> {
    let mut env = HashMap::with_capacity(agent.env.len());
    for entry in &agent.env {
        let (var_name, value) = crate::config::resolve_env_entry("[agent].env", entry, secrets)?;
        env.insert(var_name, value);
    }
    Ok(env)
}

pub(crate) fn resolve_session_cwd(raw: Option<String>, workspace_root: &str) -> Result<String> {
    let candidate = raw.unwrap_or_else(|| workspace_root.to_owned());
    let root_path = PathBuf::from(workspace_root);
    let candidate_path = PathBuf::from(&candidate);
    if !candidate_path.is_absolute() {
        return Err(StackError::PromptBodyInvalid(
            "session cwd must be an absolute path".to_owned(),
        ));
    }
    if candidate_path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(StackError::PromptBodyInvalid(
            "session cwd must not contain `..` segments".to_owned(),
        ));
    }
    let canonical_root = root_path.canonicalize().map_err(|_| {
        StackError::PromptBodyInvalid("workspace.root must be an existing directory".to_owned())
    })?;
    let canonical_candidate = candidate_path.canonicalize().map_err(|_| {
        StackError::PromptBodyInvalid(
            "session cwd must be an existing directory under workspace.root".to_owned(),
        )
    })?;
    if !canonical_candidate.is_dir() {
        return Err(StackError::PromptBodyInvalid(
            "session cwd must be an existing directory".to_owned(),
        ));
    }
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(StackError::PromptBodyInvalid(format!(
            "session cwd must be under workspace.root ({workspace_root})"
        )));
    }
    Ok(canonical_candidate.to_string_lossy().into_owned())
}

pub(super) fn stored_or_workspace_cwd(stored: &str, workspace_root: &str) -> String {
    if stored.is_empty() {
        workspace_root.to_owned()
    } else {
        stored.to_owned()
    }
}

pub(super) fn reject_closed_session(session: &SessionRecord) -> Result<()> {
    if session.status == SESSION_STATUS_CLOSED {
        return Err(StackError::SessionClosed {
            id: session.id.clone(),
        });
    }
    Ok(())
}

pub(super) fn resolve_agent_cwd(agent: &AgentConfig, workspace_root: &str) -> PathBuf {
    agent
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(workspace_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_agent_env_supports_templated_entries() {
        let home = tempfile::tempdir().expect("tempdir");
        let mut secrets = crate::secrets::SecretStore::open_or_create(home.path()).expect("store");
        secrets
            .set_many([("PLAIN", "p1"), ("TOK", "t1")])
            .expect("set secrets");
        let agent = AgentConfig {
            id: "test".to_owned(),
            name: "Test".to_owned(),
            command: "test".to_owned(),
            args: Vec::new(),
            cwd: None,
            env: vec!["PLAIN".to_owned(), "AUTH=Bearer ${TOK}".to_owned()],
            expected_sha256: None,
            restart: "on-crash".to_owned(),
            mode: None,
            model: None,
            harness_version: None,
            adapter: None,
            provider: None,
            providers: None,
            subagent: None,
            auto_update: None,
            install: None,
        };

        let env = resolve_agent_env(&agent, &secrets).expect("resolve");

        assert_eq!(env["PLAIN"], "p1");
        assert_eq!(env["AUTH"], "Bearer t1");
        assert!(!env.contains_key("TOK"));
    }
}
