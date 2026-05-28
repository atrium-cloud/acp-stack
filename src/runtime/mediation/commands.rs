//! Command Gateway — daemon-mediated shell execution.
//!
//! Responsibilities:
//!   * Resolve a submitted command against `[permissions]` policy (deny/review
//!     glob lists) before any subprocess is spawned.
//!   * Spawn a child via `workspace.default_shell -c <cmd>` with cwd resolved
//!     under `workspace.root` and env restricted to `commands.env_allowlist`.
//!     Process-group leader so a hung grandchild is reaped on cancel/timeout.
//!   * Stream stdout/stderr as `command.stdout` / `command.stderr` events into
//!     SQLite via `StateStore::append_command_output`. Each chunk is also fed
//!     to the `commands.{id}` WebSocket topic. A per-command byte cap stops
//!     persistence (but not draining) once exceeded.
//!   * Track running commands so `POST /v1/commands/{id}/cancel` can SIGTERM
//!     the process group, wait `commands.cancel_grace`, then SIGKILL.
//!
//! What this is NOT: a permissions-approval queue. Phase 1 only honors static
//! `deny` and `review` glob lists. Full review/approval lands later.
//!
//! Layout: the root file keeps `SubmitRequest` + `CommandGateway` (the public
//! surface) and the cross-cutting `RunningCommand` registry value. The
//! supervisor task, output streaming primitives, policy evaluation, and
//! process-control helpers live in sibling submodules.

mod output;
mod policy;
mod process;
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

/// Inputs for `CommandGateway::submit`. Mirror the HTTP request body shape
/// (`docs/specs/api/api.md#commands`), pre-parsed by the handler.
#[derive(Debug, Clone)]
pub struct SubmitRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub timeout_override: Option<String>,
}

/// Live registry entry the gateway keeps per running command. Owned by the
/// gateway; the supervisor task only needs to remove its own entry on exit.
/// Shared between this root and `supervisor.rs` via `pub(super)`.
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
    /// Map command id → pending permission id, so cancel() can also cancel the
    /// permission row when a caller cancels a command that is still awaiting
    /// approval. Cleared by the supervisor task once the decision lands.
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
    /// Returns the freshly-inserted record (status = pending → running once
    /// the supervisor confirms the spawn).
    pub async fn submit(&self, request: SubmitRequest) -> Result<CommandRecord> {
        // 1. Policy. `deny` rejects synchronously; `review`/`locked` route
        //    through the permission pipeline so an out-of-band approver can
        //    decide before the subprocess is spawned. The row is still
        //    inserted in `pending` so the caller has an id to poll/cancel.
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
            PolicyDecision::Allow => mode == "locked",
        };

        // 2. cwd resolution under workspace.root (must stay inside).
        let resolved_cwd = match &request.cwd {
            Some(cwd) => Some(resolve_cwd_under_workspace(
                Path::new(&self.config.workspace.root),
                cwd,
            )?),
            None => None,
        };

        // 3. env allow-list enforcement. Reject any name that is not on the
        //    configured allow-list, so submitting a request cannot inject an
        //    arbitrary env name into the child.
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

        // Persist only the env *names* in the durable row. Values commonly
        // carry credentials (API tokens, OAuth secrets); storing them in
        // SQLite would expand the secret-at-rest surface beyond the
        // age-encrypted secret store. Names are still useful for audit —
        // "this command was given $GITHUB_TOKEN" — without leaking values.
        let env_json = match &request.env {
            Some(env) if !env.is_empty() => {
                let mut names: Vec<&String> = env.keys().collect();
                names.sort(); // stable serialization for diff/audit
                Some(
                    serde_json::to_string(&names).map_err(|_| StackError::CommandDenied {
                        reason: "env names could not be serialized",
                    })?,
                )
            }
            _ => None,
        };

        // 4. Resolve per-command timeout.
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

        // 5. Insert the pending row.
        let cwd_owned = resolved_cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let record = {
            let store = self.state.lock().await;
            store.append_command(NewCommandRecord {
                command: &request.command,
                cwd: cwd_owned.as_deref(),
                env_json: env_json.as_deref(),
            })?
        };

        // 6. Register the cancel channel and spawn the supervisor.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut running = self.running.lock().await;
            running.insert(record.id.clone(), RunningCommand { cancel_tx });
        }

        // 7. If policy needs approval, create a pending permission row tied to
        //    this command. The row's `detail_json` lists the command, cwd, and
        //    env *names* — never values — so the durable record cannot leak
        //    secrets even if the events table is replicated downstream.
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
            command_id: record.id.clone(),
            shell: self.config.workspace.default_shell.clone(),
            command: request.command.clone(),
            cwd: cwd_owned,
            env: request.env.clone(),
            workspace_root: self.config.workspace.root.clone(),
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

    /// Signal the running command to cancel. The supervisor task is
    /// responsible for issuing SIGTERM, waiting `cancel_grace`, and SIGKILLing
    /// if the child has not exited. Returns the latest stored row. If the
    /// command is still awaiting a permission decision, also cancels the
    /// permission row so its durable status reflects the operator's intent.
    pub async fn cancel(&self, id: &str) -> Result<CommandRecord> {
        // Cancel the permission row first if any — the supervisor's select!
        // on perm_rx will resolve as Canceled and finalize the command row
        // without ever spawning a child.
        let perm_id = self.awaiting_permission.lock().await.remove(id);
        if let Some(perm_id) = perm_id
            && let Err(error) = self.permissions.cancel(&perm_id, "command-canceled").await
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
                    // The supervisor task dropped its receiver while we held
                    // a live entry — a race between supervisor teardown and a
                    // simultaneous cancel. Surface it: the project's
                    // error-handling rule forbids silent discard.
                    tracing::warn!(
                        error = %error,
                        command_id = %id,
                        "command cancel signal could not be delivered",
                    );
                }
            }
            None => {
                // No live supervisor: either the command never ran or it
                // already finished. Surface 404 if there is no row at all;
                // otherwise let the caller see the terminal state.
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
