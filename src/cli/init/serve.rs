use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Args;
use futures::{SinkExt, StreamExt};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{Mutex as TokioMutex, Notify, broadcast};
use tower_http::limit::RequestBodyLimitLayer;
use zeroize::{Zeroize, Zeroizing};

use crate::auth::constant_time_eq;
use crate::config;
use crate::envelope::{ApiError, ApiSuccess};
use crate::error::{Result, StackError};
use crate::fs_util::{acquire_agent_config_mutation_file_lock, home_dir};
use crate::runtime::agent::native_config_import::{NativeConfigInspection, NativeConfigSelection};
use crate::runtime::init_runner::step_kind;
use crate::state::default_state_path;

use super::prompt::{
    self, HostedPromptDriver, HostedPromptKind, HostedPromptOutcome, HostedPromptRequest,
    HostedPromptStyle,
};
use super::state_signal::{
    ApplicabilitySource, InitCategory, InitStateSignal, category_for_step_kind,
};
use super::{
    CloudflareModeArg, CloudflaredDeploymentArg, InitArgs, InitMcpHttpHeader, InitMcpHttpServer,
    InitMcpStdioServer, InitMode, InitNativeConfigUpload, run_hosted_init,
};

mod frames;
mod prompt_driver;
mod routes;
mod session;
mod state;

// Plain (non-re-exporting) globs make each sibling's `pub(super)` items private
// members of this parent module, so the other siblings and the `tests` module
// reach them via `super::NAME` / `super::*`. Nothing here escapes `serve`
// beyond `run_init_serve`/`InitServeArgs`, which stay defined in this parent.
use self::frames::*;
use self::prompt_driver::*;
use self::routes::*;
use self::session::*;
use self::state::*;

const DEFAULT_INIT_TOKEN_ENV: &str = "ACP_STACK_INIT_TOKEN";
const INIT_BOOTSTRAP_TOKEN_FIELD: &str = "bootstrap token";
const INIT_WS_CHANNEL_CAPACITY: usize = 128;
const INIT_EVENT_HISTORY_LIMIT: usize = 256;
const DEFAULT_INIT_IDLE_TIMEOUT: &str = "15m";
const IDLE_REAPER_TICK: std::time::Duration = std::time::Duration::from_secs(1);
/// How long a parked failure waits for `ack_error` before the reaper releases
/// the process. Runs regardless of `--idle-timeout` and of connected
/// WebSockets: a wedged backend holding the socket must not pin a failed
/// bootstrap forever.
const ERROR_ACK_GRACE: std::time::Duration = std::time::Duration::from_secs(2 * 60);

#[derive(Debug, Args)]
pub(super) struct InitServeArgs {
    /// Bootstrap HTTP bind address. Defaults to the normal API bind default.
    #[arg(long, default_value = config::DEFAULT_API_BIND)]
    bind: String,
    /// Environment variable containing the bootstrap bearer token.
    #[arg(long = "token-env", default_value = DEFAULT_INIT_TOKEN_ENV)]
    token_env: String,
    /// File containing the bootstrap bearer token. Overrides --token-env.
    #[arg(long = "token-file", value_name = "PATH")]
    token_file: Option<PathBuf>,
    /// Allowed browser Origin for bootstrap calls. Repeatable.
    #[arg(long = "allowed-origin")]
    allowed_origin: Vec<String>,
    /// Request body size limit for bootstrap HTTP routes.
    #[arg(long, default_value_t = super::STARTER_MAX_REQUEST_BYTES)]
    max_request_bytes: u64,
    /// Cancel the hosted init session after this long with no connected
    /// WebSocket client and no API activity (s/m/h/d/w suffix). A connected
    /// client never counts as idle. `0s` disables the reaper.
    #[arg(long, default_value = DEFAULT_INIT_IDLE_TIMEOUT)]
    idle_timeout: String,
    /// Absolute cap on server lifetime regardless of activity (s/m/h/d/w
    /// suffix). Disabled by default.
    #[arg(long)]
    max_lifetime: Option<String>,
}

pub(super) fn run_init_serve(args: InitServeArgs) -> Result<()> {
    let token = resolve_bootstrap_token(&args)?;
    let idle_timeout = parse_optional_duration(&args.idle_timeout, "idle timeout")?;
    let max_lifetime = args
        .max_lifetime
        .as_deref()
        .map(|raw| parse_optional_duration(raw, "max lifetime"))
        .transpose()?
        .flatten();
    let bind = args.bind.clone();
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
            .map(|addr| addr.to_string())
            .unwrap_or(bind);
        let state = BootstrapState {
            token: Arc::new(token),
            allowed_origins: Arc::new(args.allowed_origin),
            manager: HostedInitManager::new(),
            native_config_mutation: Arc::new(TokioMutex::new(())),
        };
        let manager = state.manager.clone();
        let shutdown_manager = state.manager.clone();
        // Always spawned: even with `--idle-timeout 0s` the loop must keep
        // running for the unconditional error-ack grace check.
        tokio::spawn(reap_idle_session(shutdown_manager.clone(), idle_timeout));
        if let Some(lifetime) = max_lifetime {
            tokio::spawn(enforce_max_lifetime(shutdown_manager.clone(), lifetime));
        }
        let app = build_bootstrap_router(state.clone(), args.max_request_bytes);
        eprintln!("acps init serve: listening on {local}");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_manager.wait_for_terminal().await;
            })
            .await
            .map_err(|source| StackError::ServeIo { source })?;
        manager.terminal_result()
    })
}

/// Parse a duration-suffix flag (`30s`, `15m`, `1h`); `0s` maps to `None`
/// (disabled). Mirrors the `acps logs --since` parsing.
fn parse_optional_duration(raw: &str, field: &'static str) -> Result<Option<std::time::Duration>> {
    let duration =
        crate::time_util::parse_duration_suffix(raw).ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("not a valid duration (use e.g. 30s, 15m, 1h): {raw}"),
        })?;
    let duration = duration.to_std().map_err(|_| StackError::InvalidParam {
        field,
        reason: format!("duration out of range: {raw}"),
    })?;
    if duration.is_zero() {
        return Ok(None);
    }
    Ok(Some(duration))
}

/// Expire the hosted session once it has been idle (no connected WebSocket
/// client and no API activity) for `timeout`. A server that never received a
/// session idles out on the same clock — measured from the last authenticated
/// API call, not just server start — so an abandoned bootstrap process cannot
/// pin the bind port indefinitely while an actively polling backend can.
/// `None` disables the idle clock but not the loop: the error-ack grace check
/// is unconditional, since an unacked parked failure would otherwise keep the
/// process alive forever.
async fn reap_idle_session(manager: Arc<HostedInitManager>, timeout: Option<std::time::Duration>) {
    loop {
        tokio::time::sleep(IDLE_REAPER_TICK).await;
        match manager.session_current() {
            Some(session) => {
                // A parked failure is owned by the ack grace alone: the idle
                // clock must not pre-empt it, so the backend is guaranteed
                // the full grace to retrieve and acknowledge the error.
                if let Some(age) = session.unacked_error_age() {
                    if age >= ERROR_ACK_GRACE {
                        session.expire("error_ack_timeout");
                        break;
                    }
                    continue;
                }
                let Some(timeout) = timeout else { continue };
                if !session.has_connected_ws()
                    && session.last_activity_age_secs() >= timeout.as_secs()
                {
                    session.expire("idle_timeout");
                    break;
                }
            }
            None => {
                let Some(timeout) = timeout else { continue };
                if manager.activity_age() >= timeout
                    && manager.shutdown_if_no_session("idle_timeout")
                {
                    break;
                }
            }
        }
    }
}

async fn enforce_max_lifetime(manager: Arc<HostedInitManager>, lifetime: std::time::Duration) {
    tokio::time::sleep(lifetime).await;
    if let Some(session) = manager.session_current()
        && session.is_active()
    {
        session.expire("max_lifetime");
        return;
    }
    manager.initiate_shutdown("max_lifetime");
}

fn resolve_bootstrap_token(args: &InitServeArgs) -> Result<String> {
    let token = if let Some(path) = args.token_file.as_ref() {
        std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
            path: path.clone(),
            source,
        })?
    } else {
        std::env::var(&args.token_env).map_err(|_| StackError::MissingField {
            field: INIT_BOOTSTRAP_TOKEN_FIELD,
        })?
    };
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(StackError::InvalidParam {
            field: INIT_BOOTSTRAP_TOKEN_FIELD,
            reason: "bootstrap token must not be empty".to_owned(),
        });
    }
    Ok(token)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInitRequest {
    agent: Option<String>,
    // Escape-hatch agent declared inline, mirroring the `--custom-agent-*`
    // flags. Its own prompts are never streamed, so the whole spec has to
    // arrive here or a hosted client cannot bring a non-registry agent at all.
    custom_agent_id: Option<String>,
    custom_agent_name: Option<String>,
    custom_agent_command: Option<String>,
    #[serde(default)]
    custom_agent_args: Vec<String>,
    custom_agent_install: Option<String>,
    custom_agent_creates: Option<String>,
    provider: Option<String>,
    api_key_ref: Option<String>,
    model: Option<String>,
    /// Declared up front like `model`, so a hosted client that already knows
    /// the session mode it wants skips the streamed picker entirely. Validated
    /// against the agent's advertised modes by the shared mode lane.
    mode: Option<String>,
    custom_provider: Option<bool>,
    provider_name: Option<String>,
    base_url: Option<String>,
    provider_api: Option<String>,
    model_name: Option<String>,
    context: Option<String>,
    output_max_tokens: Option<String>,
    workspace_root: Option<String>,
    workspace_uploads: Option<String>,
    runtime_user: Option<String>,
    sandbox: Option<String>,
    #[serde(default)]
    code_from: Vec<String>,
    #[serde(default)]
    data_from: Vec<String>,
    skip_testflight: Option<bool>,
    testflight: Option<bool>,
    native_config: Option<NativeConfigUploadRequest>,
    #[serde(default)]
    mcp_preset: Vec<String>,
    #[serde(default)]
    mcp_stdio: Vec<McpStdioServerRequest>,
    #[serde(default)]
    mcp_http: Vec<McpHttpServerRequest>,
    skills_source: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    essential_skills: Option<bool>,
    #[serde(default)]
    deps: Vec<DepRequest>,
    #[serde(default)]
    deps_system: Vec<DepRequest>,
    deps_apply: Option<bool>,
    deps_apply_yes: Option<bool>,
    standard_agent_work_deps: Option<bool>,
    browser_use: Option<bool>,
    // Update policies the interactive wizard collects after model selection.
    // They are declared up-front here rather than streamed, so the hosted flow
    // reaches the same `[updates.acp_stack]`/`[agent.auto_update]` parity as the
    // CLI's `--stack-update`/`--agent-update` flags. Absent → schema defaults.
    stack_update: Option<String>,
    stack_update_frequency: Option<String>,
    agent_update: Option<String>,
    agent_update_frequency: Option<String>,
    #[serde(default)]
    data_sources: Vec<DataSourceRequest>,
    // Run selection, matching `--resume`/`--fresh`. The interactive
    // config-source picker is not hostable (it returns unhandled and the run
    // proceeds as if nothing was chosen), so these are the only way a hosted
    // client can continue a crashed run instead of silently starting another.
    resume: Option<bool>,
    fresh: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeConfigUploadRequest {
    filename: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpStdioServerRequest {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    /// Secret ref names exported into the server's environment.
    #[serde(default)]
    env: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpHttpServerRequest {
    name: String,
    url: String,
    #[serde(default)]
    headers: Vec<McpHttpHeaderRequest>,
}

/// Exactly one of `value_ref` (whole-value secret ref) or `value`
/// (`${NAME}`-interpolated template) must be set; enforced in
/// `into_init_args` so a malformed declaration is a 400 at the boundary.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpHttpHeaderRequest {
    name: String,
    #[serde(default)]
    value_ref: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DepRequest {
    name: String,
    shell: String,
}

// A dedicated wire enum rather than `config::DataSourceConfig`: the config
// struct accepts any field combination (validation happens later in the config
// validator), while the hosted contract should reject a malformed declaration
// at the HTTP boundary and stay decoupled from the config schema.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum DataSourceRequest {
    Local {
        name: Option<String>,
        path: String,
    },
    Https {
        name: Option<String>,
        url: String,
        expected_sha256: Option<String>,
        max_download_bytes: Option<u64>,
        max_extracted_bytes: Option<u64>,
    },
    S3 {
        name: Option<String>,
        bucket: String,
        // Required here because the config validator requires it for s3
        // sources; accepting it as optional would fail the session only after
        // the boundary already returned success.
        region: String,
        prefix: Option<String>,
        access_key_ref: String,
        secret_key_ref: String,
    },
}

impl DataSourceRequest {
    fn into_data_source_config(self) -> config::DataSourceConfig {
        let mut source = config::DataSourceConfig {
            source_type: String::new(),
            name: None,
            path: None,
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        };
        match self {
            DataSourceRequest::Local { name, path } => {
                source.source_type = "local".to_owned();
                source.name = name;
                source.path = Some(path);
            }
            DataSourceRequest::Https {
                name,
                url,
                expected_sha256,
                max_download_bytes,
                max_extracted_bytes,
            } => {
                source.source_type = "https".to_owned();
                source.name = name;
                source.url = Some(url);
                source.expected_sha256 = expected_sha256;
                source.max_download_bytes = max_download_bytes;
                source.max_extracted_bytes = max_extracted_bytes;
            }
            DataSourceRequest::S3 {
                name,
                bucket,
                region,
                prefix,
                access_key_ref,
                secret_key_ref,
            } => {
                source.source_type = "s3".to_owned();
                source.name = name;
                source.bucket = Some(bucket);
                source.region = Some(region);
                source.prefix = prefix;
                source.access_key_ref = Some(access_key_ref);
                source.secret_key_ref = Some(secret_key_ref);
            }
        }
        source
    }
}

impl StartInitRequest {
    /// The clap `requires`/`conflicts_with` rules the `--custom-agent-*` flags
    /// carry, plus the two fields `resolve_custom_agent_spec` treats as
    /// mandatory, restated at the wire boundary. Reasons name fields only: a
    /// rejected declaration must never echo what was submitted, and an id or
    /// command reflected into a 400 body is the same leak as any other value.
    ///
    /// The mandatory pair is checked here as well as in the resolver because
    /// the hosted cost of deferring is not a message: an incomplete spec would
    /// start a session, hold the one-at-a-time slot, and park it errored for
    /// the full ack grace. Everything the resolver alone can judge (reserved
    /// and registry ids) still fails there, where the registry is in hand.
    fn validate_custom_agent_declaration(&self) -> Result<()> {
        let dependents: [(&'static str, bool); 5] = [
            ("custom_agent_name", self.custom_agent_name.is_some()),
            ("custom_agent_command", self.custom_agent_command.is_some()),
            ("custom_agent_args", !self.custom_agent_args.is_empty()),
            ("custom_agent_install", self.custom_agent_install.is_some()),
            ("custom_agent_creates", self.custom_agent_creates.is_some()),
        ];
        if self.custom_agent_id.is_none() {
            if let Some((field, _)) = dependents.into_iter().find(|(_, present)| *present) {
                return Err(StackError::InvalidParam {
                    field,
                    reason: format!("{field} requires custom_agent_id"),
                });
            }
            return Ok(());
        }
        // A custom agent is configured through its own environment, so every
        // registry-driven harness knob is meaningless for it. Booleans are
        // judged on their effective value: an explicit `false` declares
        // nothing and must not collide.
        let conflicts: [(&'static str, bool); 5] = [
            ("agent", self.agent.is_some()),
            ("provider", self.provider.is_some()),
            ("model", self.model.is_some()),
            ("mode", self.mode.is_some()),
            ("custom_provider", self.custom_provider.unwrap_or(false)),
        ];
        if let Some((field, _)) = conflicts.into_iter().find(|(_, present)| *present) {
            return Err(StackError::InvalidParam {
                field: "custom_agent_id",
                reason: format!("custom_agent_id conflicts with {field}"),
            });
        }
        // Blank counts as absent, matching `require_custom_flag`: a spec that
        // cannot launch or install the agent is not a spec.
        let declared = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        };
        for (field, present) in [
            ("custom_agent_command", declared(&self.custom_agent_command)),
            ("custom_agent_install", declared(&self.custom_agent_install)),
        ] {
            if !present {
                return Err(StackError::InvalidParam {
                    field,
                    reason: format!("custom_agent_id requires {field}"),
                });
            }
        }
        Ok(())
    }

    fn into_init_args(self) -> Result<InitArgs> {
        // Only what clap or the engine cannot structurally catch: clap's
        // `requires`/`conflicts_with` declarations have no hosted equivalent,
        // and dep names round-trip through a `NAME=SHELL` string split.
        // Both-or-neither, stricter than the CLI's `requires`: the hosted
        // driver never streams the interactive apply confirmation, so
        // `deps_apply` alone would silently default to "not applied".
        if self.deps_apply.unwrap_or(false) != self.deps_apply_yes.unwrap_or(false) {
            return Err(StackError::InvalidParam {
                field: "deps_apply",
                reason: "deps_apply and deps_apply_yes must be set together".to_owned(),
            });
        }
        // Mirror clap's `requires` on the CLI frequency flags: a frequency with
        // no policy would be silently dropped by the configure step, so reject
        // it at the boundary instead. Value validation (on|security|off, unit
        // limits, custom-agent rejection) runs later in the shared engine via
        // `validate_stack_update_args`/`validate_agent_update_args`.
        if self.stack_update_frequency.is_some() && self.stack_update.is_none() {
            return Err(StackError::InvalidParam {
                field: "stack_update_frequency",
                reason: "stack_update_frequency requires stack_update".to_owned(),
            });
        }
        if self.agent_update_frequency.is_some() && self.agent_update.is_none() {
            return Err(StackError::InvalidParam {
                field: "agent_update_frequency",
                reason: "agent_update_frequency requires agent_update".to_owned(),
            });
        }
        self.validate_custom_agent_declaration()?;
        // Mirror clap's `requires` on the provider family. Provider processing
        // returns early when no provider is declared and the custom-provider
        // fields are read only while assembling one, so an unanchored field
        // would be dropped without a word. Ordered after the custom-agent
        // declaration so a request that both names a custom agent and asks for
        // a custom provider still reports the conflict that actually explains
        // it.
        let custom_provider = self.custom_provider.unwrap_or(false);
        if self.provider.is_none() {
            for (field, declared) in [
                ("api_key_ref", self.api_key_ref.is_some()),
                ("custom_provider", custom_provider),
            ] {
                if declared {
                    return Err(StackError::InvalidParam {
                        field,
                        reason: format!("{field} requires provider"),
                    });
                }
            }
        }
        if !custom_provider {
            for (field, declared) in [
                ("provider_name", self.provider_name.is_some()),
                ("base_url", self.base_url.is_some()),
                ("provider_api", self.provider_api.is_some()),
                ("model_name", self.model_name.is_some()),
                ("context", self.context.is_some()),
                ("output_max_tokens", self.output_max_tokens.is_some()),
            ] {
                if declared {
                    return Err(StackError::InvalidParam {
                        field,
                        reason: format!("{field} requires custom_provider"),
                    });
                }
            }
        }
        // Mirrors clap's `conflicts_with` between `--resume` and `--fresh`:
        // one says continue the recorded run, the other says ignore it.
        let resume = self.resume.unwrap_or(false);
        let fresh = self.fresh.unwrap_or(false);
        if resume && fresh {
            return Err(StackError::InvalidParam {
                field: "resume",
                reason: "resume conflicts with fresh".to_owned(),
            });
        }
        let essential_skills = self.essential_skills.unwrap_or(false);
        if essential_skills && (self.skills_source.is_some() || !self.skills.is_empty()) {
            return Err(StackError::InvalidParam {
                field: "essential_skills",
                reason: "essential_skills conflicts with skills_source/skills".to_owned(),
            });
        }
        if self.skills_source.is_some() == self.skills.is_empty() {
            return Err(StackError::InvalidParam {
                field: "skills",
                reason: "skills and skills_source must be declared together".to_owned(),
            });
        }
        for dep in self.deps.iter().chain(self.deps_system.iter()) {
            if dep.name.trim().is_empty() || dep.shell.trim().is_empty() {
                return Err(StackError::InvalidParam {
                    field: "deps",
                    reason: "dependency name and shell must not be empty".to_owned(),
                });
            }
            if dep.name.contains('=') {
                return Err(StackError::InvalidParam {
                    field: "deps",
                    reason: format!("dependency name `{}` must not contain `=`", dep.name),
                });
            }
        }
        let mut args = empty_init_args();
        args.agent = self.agent;
        args.custom_agent_id = self.custom_agent_id;
        args.custom_agent_name = self.custom_agent_name;
        args.custom_agent_command = self.custom_agent_command;
        args.custom_agent_arg = self.custom_agent_args;
        args.custom_agent_install = self.custom_agent_install;
        args.custom_agent_creates = self.custom_agent_creates;
        args.resume = resume;
        args.fresh = fresh;
        args.provider = self.provider;
        args.api_key_ref = self.api_key_ref;
        args.model = self.model;
        args.mode = self.mode;
        args.custom_provider = custom_provider;
        args.provider_name = self.provider_name;
        args.base_url = self.base_url;
        args.provider_api = self.provider_api;
        args.model_name = self.model_name;
        args.context = self.context;
        args.output_max_tokens = self.output_max_tokens;
        args.workspace_root = self.workspace_root;
        args.workspace_uploads = self.workspace_uploads;
        args.runtime_user = self.runtime_user;
        args.sandbox = self.sandbox;
        args.code_from = self.code_from;
        args.data_from = self.data_from;
        args.skip_testflight = self.skip_testflight.unwrap_or(false);
        args.testflight = self.testflight.unwrap_or(false);
        args.native_config_upload = self.native_config.map(|upload| InitNativeConfigUpload {
            filename: upload.filename,
            content: Zeroizing::new(upload.content),
        });
        args.mcp_preset = self.mcp_preset;
        // Structured records land on the wizard-side prompt_* fields, which
        // are strictly more expressive than the NAME=VALUE flag strings (argv
        // and env for stdio servers); `mcp_from_args` merges and validates
        // them the same way.
        // Boundary validation runs screening before any name-shape check: a
        // screening rejection redacts a pasted credential, while name-shape
        // errors echo the offending string into the 400 body.
        args.prompt_mcp_stdio = self
            .mcp_stdio
            .into_iter()
            .map(|server| {
                for entry in &server.env {
                    crate::config::screen_env_entry("mcp_stdio.env", entry)
                        .and_then(|()| crate::config::parse_env_entry("mcp_stdio.env", entry))
                        .map_err(|error| StackError::InvalidParam {
                            field: "mcp_stdio.env",
                            reason: error.to_string(),
                        })?;
                }
                Ok(InitMcpStdioServer {
                    name: server.name,
                    command: server.command,
                    args: server.args,
                    env: server.env,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        args.prompt_mcp_http = self
            .mcp_http
            .into_iter()
            .map(|server| {
                let headers = server
                    .headers
                    .into_iter()
                    .map(|header| {
                        match (header.value_ref.as_deref(), header.value.as_deref()) {
                            (Some(value_ref), None) => {
                                crate::config::screen_ref_name("mcp_http.headers", value_ref)
                                    .and_then(|()| {
                                        crate::config::validate_secret_ref_name_value(value_ref)
                                    })
                                    .map_err(|error| StackError::InvalidParam {
                                        field: "mcp_http.headers",
                                        reason: error.to_string(),
                                    })?;
                            }
                            (None, Some(template)) => {
                                crate::config::screen_template("mcp_http.headers", template)
                                    .and_then(|()| {
                                        crate::config::SecretTemplate::parse(
                                            "mcp_http.headers",
                                            template,
                                        )
                                        .map(|_| ())
                                    })
                                    .map_err(|error| StackError::InvalidParam {
                                        field: "mcp_http.headers",
                                        reason: error.to_string(),
                                    })?;
                            }
                            _ => {
                                return Err(StackError::InvalidParam {
                                    field: "mcp_http.headers",
                                    reason: format!(
                                        "header `{}` must set exactly one of `value_ref` or `value`",
                                        header.name
                                    ),
                                });
                            }
                        }
                        Ok(InitMcpHttpHeader {
                            name: header.name,
                            value_ref: header.value_ref,
                            value: header.value,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(InitMcpHttpServer {
                    name: server.name,
                    url: server.url,
                    headers,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // `empty_init_args` defaults to `no_skills: true` and the skill plan
        // resolver short-circuits on it, so any skills declaration must clear
        // it or the declaration would be silently dropped.
        //
        // A resume that redeclares nothing must not inherit that default: the
        // recorded skills replay is itself gated on `!no_skills`, so leaving it
        // set would drop the original run's skill plan — and a run that crashed
        // inside `agent_skills_install` would then resume into a step with no
        // plan to re-drive and fail as a corrupted run.
        args.no_skills =
            !resume && self.skills_source.is_none() && self.skills.is_empty() && !essential_skills;
        args.skills_source = self.skills_source;
        args.skills = self.skills;
        args.essential_skills = essential_skills;
        // Same `NAME=SHELL` shape the wizard pushes, so `deps_from_args`
        // consumes flag, wizard, and hosted declarations uniformly.
        args.dep = self
            .deps
            .iter()
            .map(|dep| format!("{}={}", dep.name, dep.shell))
            .collect();
        args.dep_system = self
            .deps_system
            .iter()
            .map(|dep| format!("{}={}", dep.name, dep.shell))
            .collect();
        args.deps_apply = self.deps_apply.unwrap_or(false);
        args.deps_apply_yes = self.deps_apply_yes.unwrap_or(false);
        args.standard_agent_work_deps = self.standard_agent_work_deps.unwrap_or(false);
        args.browser_use_profile = self.browser_use.unwrap_or(false);
        args.stack_update = self.stack_update;
        args.stack_update_frequency = self.stack_update_frequency;
        args.agent_update = self.agent_update;
        args.agent_update_frequency = self.agent_update_frequency;
        args.prompt_data_sources = self
            .data_sources
            .into_iter()
            .map(DataSourceRequest::into_data_source_config)
            .collect();
        Ok(args)
    }
}

fn empty_init_args() -> InitArgs {
    InitArgs {
        agent: None,
        custom_agent_id: None,
        custom_agent_name: None,
        custom_agent_command: None,
        custom_agent_arg: Vec::new(),
        custom_agent_install: None,
        custom_agent_creates: None,
        agent_env_ref: Vec::new(),
        dep: Vec::new(),
        dep_system: Vec::new(),
        deps_apply: false,
        deps_apply_yes: false,
        stack_update: None,
        stack_update_frequency: None,
        agent_update: None,
        agent_update_frequency: None,
        non_interactive: false,
        handoff_json: false,
        from_file: None,
        from_toml: None,
        from_base64: None,
        provider: None,
        api_key_ref: None,
        custom_provider: false,
        provider_name: None,
        base_url: None,
        provider_api: None,
        model: None,
        mode: None,
        model_name: None,
        context: None,
        output_max_tokens: None,
        skills_source: None,
        skills: Vec::new(),
        essential_skills: false,
        no_skills: true,
        edge: None,
        exposure: None,
        hostname: None,
        cloudflare_mode: CloudflareModeArg::Generated,
        cloudflare_api_token_ref: None,
        cloudflare_account_id_ref: None,
        cloudflared_deployment: CloudflaredDeploymentArg::Host,
        workspace_root: None,
        workspace_uploads: None,
        runtime_user: None,
        sandbox: None,
        code_from: Vec::new(),
        data_from: Vec::new(),
        mcp_preset: Vec::new(),
        mcp_stdio: Vec::new(),
        mcp_stdio_env: Vec::new(),
        mcp_http: Vec::new(),
        mcp_http_header: Vec::new(),
        supabase_url: None,
        supabase_schema: None,
        supabase_api_key_ref: None,
        no_supabase: false,
        #[cfg(feature = "dev-tools")]
        skip_workspace_init: false,
        testflight: false,
        native_config_upload: None,
        native_config_revision: None,
        skip_testflight: false,
        standard_agent_work_deps: false,
        browser_use_profile: false,
        prompt_agent_env_refs: false,
        prompt_skills: false,
        prompt_data_sources: Vec::new(),
        prompt_mcp_stdio: Vec::new(),
        prompt_mcp_http: Vec::new(),
        resume: false,
        fresh: false,
        run_id: None,
        // No request field: hosted mode forces this true at init entry and
        // records it, so any resume of a crashed hosted run re-rotates — the
        // `resume` request field included, which reaches the same replay the
        // CLI's `--resume` does and rotates exactly once per resumed run.
        rotate_keys: false,
    }
}

#[derive(Debug, Serialize)]
struct StartInitResponse {
    session_id: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct SimpleSessionResponse {
    session_id: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct InitEventsResponse {
    session_id: String,
    events: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct InitStatusResponse {
    session_id: String,
    status: String,
    /// Category snapshot, identical in shape to the `state` frame and to the
    /// `state` field of `hello`, so a REST poller and a socket client read the
    /// same thing.
    state: StateSnapshot,
    last_seq: u64,
    pending_input: Option<PublicInputRequest>,
    recent_events: Vec<Value>,
    result_available: bool,
    error: Option<PublicError>,
    last_activity_age_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct PublicError {
    code: String,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct PublicInputRequest {
    request_id: String,
    /// Machine-readable prompt identity, from `HostedPromptKind::as_str`. Field
    /// order here is the wire order; `kind` sits beside `request_id` so a
    /// client can route on it before parsing the rest.
    kind: &'static str,
    style: String,
    prompt: String,
    required: bool,
    default: Option<bool>,
    options: Vec<PublicInputOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inspection: Option<NativeConfigInspection>,
}

#[derive(Debug, Clone, Serialize)]
struct PublicInputOption {
    index: usize,
    /// Stable option id an answer may address by `{"value": "<id>"}`; unlike
    /// `label` it survives display rewording.
    value: String,
    label: String,
    hint: String,
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
