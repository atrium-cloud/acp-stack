//! Command Gateway: daemon-mediated shell execution, from `[permissions]`
//! policy resolution through spawn, output streaming, and cancellation.

pub(crate) mod exec;
pub(crate) mod output;
pub(crate) mod policy;
pub(crate) mod process;
mod supervisor;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::{Mutex as TokioMutex, watch};

use crate::config::{Config, parse_duration_string};
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::runtime::mediation::permissions::{NewPermission, PermissionService, PermissionSource};
use crate::state::{CommandRecord, NewCommandRecord, StateStore};

use self::policy::{PolicyDecision, evaluate_policy, resolve_cwd_under_workspace};
use self::supervisor::SupervisorTask;

/// Decision reasons recorded when command teardown settles a still-pending
/// permission row; only `PERMISSION_REASON_WAITER_LOST` names a real anomaly.
pub(crate) const PERMISSION_REASON_COMMAND_CANCELED: &str = "command-cancelled";
pub(crate) const PERMISSION_REASON_DENIED: &str = "command-permission-denied";
pub(crate) const PERMISSION_REASON_WAITER_LOST: &str = "command-permission-waiter-lost";
pub(crate) const PERMISSION_REASON_START_FAILED: &str = "command-start-failed";
pub(crate) const PERMISSION_REASON_SPAWN_FAILED: &str = "command-spawn-failed";
pub(crate) const PERMISSION_REASON_PERSISTENCE_FAILED: &str = "command-persistence-failed";
pub(crate) const PERMISSION_REASON_COMMAND_FINISHED: &str = "command-finished";

/// Inputs for `CommandGateway::submit`, mirroring the HTTP request body in
/// `docs/specs/api/api.md#commands`.
#[derive(Debug, Clone)]
pub struct SubmitRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub timeout_override: Option<String>,
}

/// Live registry entry the gateway keeps per running command.
pub(super) struct RunningCommand {
    pub(super) cancel_tx: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct CommandGateway {
    state: Arc<TokioMutex<StateStore>>,
    event_hub: EventHub,
    config: Arc<Config>,
    running: Arc<TokioMutex<HashMap<String, RunningCommand>>>,
    permissions: PermissionService,
    /// Command id → pending permission id, so `cancel` can settle the
    /// permission row too. Cleared by the supervisor once the decision lands.
    awaiting_permission: Arc<TokioMutex<HashMap<String, String>>>,
}

impl CommandGateway {
    pub fn new(
        state: Arc<TokioMutex<StateStore>>,
        event_hub: EventHub,
        config: Arc<Config>,
        permissions: PermissionService,
    ) -> Self {
        Self {
            state,
            event_hub,
            config,
            running: Arc::new(TokioMutex::new(HashMap::new())),
            permissions,
            awaiting_permission: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    /// Validate, persist a `commands` row, and spawn the supervisor task.
    pub async fn submit(&self, request: SubmitRequest) -> Result<CommandRecord> {
        let decision = evaluate_policy(&request.command, &self.config.permissions);
        let mode = self.config.permissions.mode.as_str();
        let review_flagged = matches!(decision, PolicyDecision::Review) && mode == "auto";
        let needs_approval = match decision {
            PolicyDecision::Deny => {
                return Err(StackError::CommandDenied {
                    reason: "matched [permissions].deny pattern",
                });
            }
            PolicyDecision::Review => mode == "supervised" || mode == "locked",
            PolicyDecision::ReviewRequired => true,
            PolicyDecision::Allow => mode == "locked",
        };

        let workspace_root_path = Path::new(&self.config.workspace.root);
        let execution_cwd = match &request.cwd {
            Some(cwd) => resolve_cwd_under_workspace(workspace_root_path, cwd)?,
            None => resolve_cwd_under_workspace(workspace_root_path, &self.config.workspace.root)?,
        };

        if let Some(env) = &request.env {
            for name in env.keys() {
                if !self
                    .config
                    .commands
                    .env_allowlist
                    .iter()
                    .any(|allowed| allowed == name)
                {
                    return Err(StackError::CommandEnvNotAllowed { name: name.clone() });
                }
            }
        }

        // Persist only env names: values commonly carry credentials, and the
        // durable row must not widen the secret-at-rest surface.
        let env_json = match &request.env {
            Some(env) if !env.is_empty() => {
                let mut names: Vec<&String> = env.keys().collect();
                names.sort();
                Some(
                    serde_json::to_string(&names).map_err(|_| StackError::CommandDenied {
                        reason: "env names could not be serialized",
                    })?,
                )
            }
            _ => None,
        };

        let timeout_duration = match &request.timeout_override {
            Some(text) => parse_duration_string(text).ok_or(StackError::InvalidDurationField {
                field: "command.timeout",
            })?,
            None => parse_duration_string(&self.config.commands.default_timeout).ok_or(
                StackError::InvalidDurationField {
                    field: "commands.default_timeout",
                },
            )?,
        };

        let cancel_grace = parse_duration_string(&self.config.commands.cancel_grace).ok_or(
            StackError::InvalidDurationField {
                field: "commands.cancel_grace",
            },
        )?;
        let progress_interval = parse_duration_string(&self.config.commands.progress_interval)
            .ok_or(StackError::InvalidDurationField {
                field: "commands.progress_interval",
            })?;

        let cwd_owned = request.cwd.as_ref().map(|_| execution_cwd.display_path());
        let record = {
            let store = self.state.lock().await;
            store.append_command(NewCommandRecord {
                command: &request.command,
                cwd: cwd_owned.as_deref(),
                env_json: env_json.as_deref(),
                origin: crate::state::CommandOrigin::Operator,
                session_id: None,
            })?
        };

        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut running = self.running.lock().await;
            running.insert(record.id.clone(), RunningCommand { cancel_tx });
        }

        // `detail_json` lists env names, never values, so a replicated events
        // table cannot leak secrets.
        let pending_permission = if needs_approval {
            let env_names: Vec<String> = request
                .env
                .as_ref()
                .map(|env| {
                    let mut names: Vec<String> = env.keys().cloned().collect();
                    names.sort();
                    names
                })
                .unwrap_or_default();
            let (perm_record, perm_rx) = self
                .permissions
                .request(NewPermission {
                    source: PermissionSource::Command,
                    requester: Some(format!("command:{}", record.id)),
                    subject_id: Some(record.id.clone()),
                    detail: json!({
                        "command": request.command,
                        "cwd": cwd_owned,
                        "env_names": env_names,
                        "policy_decision": match decision {
                            PolicyDecision::Review => "review",
                            PolicyDecision::ReviewRequired => "shell-composition",
                            PolicyDecision::Allow => "locked-default",
                            PolicyDecision::Deny => "deny",
                        },
                    }),
                })
                .await?;
            self.awaiting_permission
                .lock()
                .await
                .insert(record.id.clone(), perm_record.id.clone());
            Some(perm_rx)
        } else {
            None
        };

        let task = SupervisorTask {
            state: self.state.clone(),
            event_hub: self.event_hub.clone(),
            running: self.running.clone(),
            awaiting_permission: self.awaiting_permission.clone(),
            permissions: self.permissions.clone(),
            command_id: record.id.clone(),
            shell: self.config.workspace.default_shell.clone(),
            command: request.command.clone(),
            sandbox: self.config.workspace.sandbox.clone(),
            network_provider: crate::extensions::resolve_network_provider(&self.config),
            workspace_root: std::path::PathBuf::from(&self.config.workspace.root),
            cwd: execution_cwd,
            env: request.env.clone(),
            timeout_duration,
            cancel_grace,
            progress_interval,
            cancel_rx,
            max_output_bytes: self.config.commands.max_output_bytes as usize,
            review_flagged,
            permission_rx: pending_permission,
        };
        tokio::spawn(task.run());

        Ok(record)
    }

    pub async fn get(&self, id: &str) -> Result<CommandRecord> {
        let store = self.state.lock().await;
        store
            .get_command(id)?
            .ok_or_else(|| StackError::CommandNotFound { id: id.to_owned() })
    }

    pub async fn list(&self, limit: u32) -> Result<Vec<CommandRecord>> {
        let store = self.state.lock().await;
        store.query_commands(crate::state::CommandFilter {
            limit,
            ..Default::default()
        })
    }

    /// Signal the running command to cancel and return the latest stored row.
    pub async fn cancel(&self, id: &str) -> Result<CommandRecord> {
        // Cancel the permission row first so the supervisor settles the command
        // without spawning a child. Read, don't take: the supervisor's
        // deregister owns removal, and taking here would orphan the row if this
        // cancel errors out.
        let perm_id = self.awaiting_permission.lock().await.get(id).cloned();
        if let Some(perm_id) = perm_id
            && let Err(error) = self
                .permissions
                .cancel_if_pending(&perm_id, PERMISSION_REASON_COMMAND_CANCELED)
                .await
        {
            tracing::warn!(
                error = %error,
                command_id = %id,
                permission_id = %perm_id,
                "failed to cancel pending permission alongside command cancel",
            );
        }
        let sender = {
            let running = self.running.lock().await;
            running.get(id).map(|entry| entry.cancel_tx.clone())
        };
        match sender {
            Some(tx) => {
                if let Err(error) = tx.send(true) {
                    tracing::warn!(
                        error = %error,
                        command_id = %id,
                        "command cancel signal could not be delivered",
                    );
                }
            }
            None => {
                let store = self.state.lock().await;
                return store
                    .get_command(id)?
                    .ok_or_else(|| StackError::CommandNotFound { id: id.to_owned() });
            }
        }
        let store = self.state.lock().await;
        store
            .get_command(id)?
            .ok_or_else(|| StackError::CommandNotFound { id: id.to_owned() })
    }
}
