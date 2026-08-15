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
mod tests {
    use super::*;

    use crate::runtime::init_runner::StepDisposition;
    use crate::secrets::SecretStore;
    // The two init-side seams the MCP hosted lift exercises: the wizard the
    // `mcp_configure` step drives, and the secret collection that follows it.
    use super::super::provider::collect_mcp_secret_refs_for_init;
    use super::super::starter_config::{mcp_servers_from_prompted, prompt_mcp_servers};

    use axum::body::to_bytes;
    use http::{Method, Request};
    // Frame construction moved to `frames.rs`, so `json!` is now only used to
    // build test fixtures and expected values.
    use serde_json::json;
    use std::time::Duration;
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "test_bootstrap_token";

    fn test_session(id: &str) -> Arc<HostedInitSession> {
        HostedInitSession::new(id.to_owned(), Arc::new(Notify::new()))
    }

    fn wait_for_pending_input(session: &HostedInitSession) -> PublicInputRequest {
        for _ in 0..100 {
            if let Some(input) = lock_unpoisoned(&session.inner).pending_input.clone() {
                return input;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for hosted init input request");
    }

    /// Option ids are derived from the labels so the wire `value` is stable and
    /// distinct from the display text, exactly as the real call sites build them.
    fn hosted_items(labels: &[&str]) -> Vec<prompt::HostedPromptItem> {
        labels
            .iter()
            .map(|label| prompt::HostedPromptItem {
                value: format!("id_{label}"),
                label: (*label).to_owned(),
                hint: String::new(),
            })
            .collect()
    }

    fn hosted_test_request(
        kind: HostedPromptKind,
        style: HostedPromptStyle,
        prompt: &str,
        labels: &[&str],
    ) -> HostedPromptRequest {
        HostedPromptRequest {
            kind,
            style,
            prompt: prompt.to_owned(),
            required: false,
            default: None,
            items: hosted_items(labels),
            inspection: None,
        }
    }

    /// Drives one select to completion and hands back the raw driver result, so
    /// rejection paths stay assertable.
    fn select_result(
        kind: HostedPromptKind,
        prompt: &str,
        labels: &[&str],
        response: Value,
    ) -> Result<HostedPromptOutcome<Option<usize>>> {
        let session = test_session("init_driver_select");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request =
            hosted_test_request(kind, HostedPromptStyle::SearchableSelect, prompt, labels);
        let handle = std::thread::spawn(move || driver.select(request));
        let pending = wait_for_pending_input(&session);
        session
            .submit_input(&pending.request_id, response)
            .expect("submit input");
        handle.join().expect("driver thread")
    }

    fn send_select_response(
        kind: HostedPromptKind,
        prompt: &str,
        labels: &[&str],
        response: Value,
    ) -> HostedPromptOutcome<Option<usize>> {
        select_result(kind, prompt, labels, response).expect("driver result")
    }

    #[test]
    fn start_init_request_maps_sandbox_into_args() {
        // `deny_unknown_fields` means the platform payload is rejected outright
        // unless `sandbox` is a known field; this also covers the arg mapping.
        let request: StartInitRequest =
            serde_json::from_str(r#"{"agent":"placebo","sandbox":"unshare"}"#)
                .expect("sandbox must be an accepted request field");
        let args = request.into_init_args().expect("valid request");
        assert_eq!(args.sandbox.as_deref(), Some("unshare"));
        // The wire spells the repeatable custom-agent argument list in the
        // plural; the CLI's singular `--custom-agent-arg` is not a field name,
        // and `deny_unknown_fields` has to keep saying so.
        assert!(
            serde_json::from_str::<StartInitRequest>(r#"{"custom_agent_args":["--stdio"]}"#)
                .is_ok(),
            "custom_agent_args must be an accepted request field"
        );
        assert!(
            serde_json::from_str::<StartInitRequest>(r#"{"custom_agent_arg":["--stdio"]}"#)
                .is_err(),
            "the singular CLI flag spelling must not be accepted"
        );
    }

    fn request_from_json(payload: &str) -> StartInitRequest {
        serde_json::from_str(payload).expect("request payload must deserialize")
    }

    #[test]
    fn start_init_request_maps_custom_agent_declaration_into_args() {
        let args = request_from_json(
            r#"{
                "custom_agent_id": "housebot",
                "custom_agent_name": "House Bot",
                "custom_agent_command": "housebot-acp",
                "custom_agent_args": ["--stdio", "--quiet"],
                "custom_agent_install": "npm install -g housebot",
                "custom_agent_creates": "/usr/local/bin/housebot-acp"
            }"#,
        )
        .into_init_args()
        .expect("valid request");
        assert_eq!(args.custom_agent_id.as_deref(), Some("housebot"));
        assert_eq!(args.custom_agent_name.as_deref(), Some("House Bot"));
        assert_eq!(args.custom_agent_command.as_deref(), Some("housebot-acp"));
        assert_eq!(
            args.custom_agent_arg,
            vec!["--stdio".to_owned(), "--quiet".to_owned()]
        );
        assert_eq!(
            args.custom_agent_install.as_deref(),
            Some("npm install -g housebot")
        );
        assert_eq!(
            args.custom_agent_creates.as_deref(),
            Some("/usr/local/bin/housebot-acp")
        );
        assert!(args.agent.is_none());
        // The spec assembles through the same resolver the CLI flags use.
        let spec = super::super::registry_apply::resolve_custom_agent_spec(&args)
            .expect("spec must resolve")
            .expect("a declared custom agent must produce a spec");
        assert_eq!(spec.id, "housebot");
        assert_eq!(spec.creates, "/usr/local/bin/housebot-acp");
    }

    #[test]
    fn start_init_request_rejects_custom_agent_fields_without_id() {
        // Mirrors clap's `requires = "custom_agent_id"` on each dependent flag.
        for (field, payload) in [
            ("custom_agent_name", r#"{"custom_agent_name": "House Bot"}"#),
            (
                "custom_agent_command",
                r#"{"custom_agent_command": "housebot-acp"}"#,
            ),
            ("custom_agent_args", r#"{"custom_agent_args": ["--stdio"]}"#),
            (
                "custom_agent_install",
                r#"{"custom_agent_install": "npm install -g housebot"}"#,
            ),
            (
                "custom_agent_creates",
                r#"{"custom_agent_creates": "/usr/local/bin/housebot-acp"}"#,
            ),
        ] {
            let error = request_from_json(payload)
                .into_init_args()
                .expect_err("a dependent custom-agent field needs custom_agent_id");
            match error {
                StackError::InvalidParam {
                    field: rejected,
                    ref reason,
                } => {
                    assert_eq!(rejected, field);
                    // The rejection names fields, never the submitted spec.
                    for value in ["House Bot", "housebot-acp", "--stdio", "npm install"] {
                        assert!(!reason.contains(value), "{reason} echoed a submitted value");
                    }
                }
                other => panic!("expected an InvalidParam for {field}, got {other}"),
            }
        }
    }

    #[test]
    fn start_init_request_rejects_custom_agent_conflicts_without_echoing_values() {
        for payload in [
            r#"{"custom_agent_id": "housebot", "agent": "opencode"}"#,
            r#"{"custom_agent_id": "housebot", "provider": "openrouter"}"#,
            r#"{"custom_agent_id": "housebot", "model": "openai/gpt-5"}"#,
            r#"{"custom_agent_id": "housebot", "mode": "plan"}"#,
            r#"{"custom_agent_id": "housebot", "custom_provider": true}"#,
        ] {
            let error = request_from_json(payload)
                .into_init_args()
                .expect_err("registry knobs conflict with a custom agent");
            match error {
                StackError::InvalidParam { field, ref reason } => {
                    assert_eq!(field, "custom_agent_id");
                    for value in ["housebot", "opencode", "openrouter", "openai/gpt-5", "plan"] {
                        assert!(!reason.contains(value), "{reason} echoed a submitted value");
                    }
                }
                other => panic!("expected an InvalidParam, got {other}"),
            }
        }
        // An explicitly false boolean declares nothing, so it must not collide.
        let benign = request_from_json(
            r#"{"custom_agent_id": "housebot", "custom_provider": false,
                "custom_agent_command": "housebot-acp",
                "custom_agent_install": "npm install -g housebot"}"#,
        )
        .into_init_args()
        .expect("custom_provider:false is not a declaration");
        assert!(!benign.custom_provider);
    }

    #[test]
    fn start_init_request_rejects_a_custom_agent_that_cannot_launch_or_install() {
        // The resolver treats both as mandatory. Rejecting here instead of at
        // the resolver keeps an incomplete spec from consuming the session slot
        // and parking it errored for the ack grace.
        for (field, payload) in [
            ("custom_agent_command", r#"{"custom_agent_id": "housebot"}"#),
            (
                "custom_agent_command",
                r#"{"custom_agent_id": "housebot", "custom_agent_install": "npm i -g housebot"}"#,
            ),
            (
                "custom_agent_install",
                r#"{"custom_agent_id": "housebot", "custom_agent_command": "housebot-acp"}"#,
            ),
            // Blank is absent, exactly as `require_custom_flag` reads it.
            (
                "custom_agent_command",
                r#"{"custom_agent_id": "housebot", "custom_agent_command": "  ",
                    "custom_agent_install": "npm i -g housebot"}"#,
            ),
            (
                "custom_agent_install",
                r#"{"custom_agent_id": "housebot", "custom_agent_command": "housebot-acp",
                    "custom_agent_install": ""}"#,
            ),
        ] {
            let error = request_from_json(payload)
                .into_init_args()
                .expect_err("an incomplete custom-agent spec must be rejected at the boundary");
            match error {
                StackError::InvalidParam {
                    field: rejected,
                    ref reason,
                } => {
                    assert_eq!(rejected, field);
                    // Request-field terms, never the CLI flag spelling, and
                    // never the submitted spec.
                    assert!(!reason.contains("--"), "{reason} names a CLI flag");
                    for value in ["housebot", "npm i -g"] {
                        assert!(!reason.contains(value), "{reason} echoed a submitted value");
                    }
                }
                other => panic!("expected an InvalidParam for {field}, got {other}"),
            }
        }
    }

    #[test]
    fn start_init_request_maps_mode_into_args() {
        // Declare-up-front parity with provider/model: a hosted client that
        // knows its mode must not have to answer the streamed picker.
        let args = request_from_json(r#"{"agent": "opencode", "mode": "plan"}"#)
            .into_init_args()
            .expect("valid request");
        assert_eq!(args.mode.as_deref(), Some("plan"));

        let absent = request_from_json(r#"{"agent": "opencode"}"#)
            .into_init_args()
            .expect("valid request");
        assert_eq!(absent.mode, None);
    }

    #[test]
    fn start_init_request_maps_resume_and_fresh() {
        let resume = request_from_json(r#"{"resume": true}"#)
            .into_init_args()
            .expect("valid request");
        assert!(resume.resume);
        assert!(!resume.fresh);
        // A resume that redeclares no skills must not inherit the hosted
        // `no_skills` default, or the recorded skill plan replay is skipped.
        assert!(!resume.no_skills);

        let fresh = request_from_json(r#"{"fresh": true}"#)
            .into_init_args()
            .expect("valid request");
        assert!(fresh.fresh);
        assert!(!fresh.resume);
        assert!(fresh.no_skills);

        let conflict = request_from_json(r#"{"resume": true, "fresh": true}"#)
            .into_init_args()
            .expect_err("resume and fresh are mutually exclusive");
        assert!(matches!(
            conflict,
            StackError::InvalidParam {
                field: "resume",
                ..
            }
        ));

        // Requests that say nothing keep the pre-existing defaults.
        let quiet = request_from_json(r#"{}"#)
            .into_init_args()
            .expect("valid request");
        assert!(!quiet.resume);
        assert!(!quiet.fresh);
        assert!(quiet.custom_agent_id.is_none());
        assert!(quiet.custom_agent_arg.is_empty());
    }

    #[test]
    fn hosted_resume_reuses_the_recorded_run_and_its_rotation() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = crate::state::StateStore::open(tempdir.path().join("state.sqlite"))
            .expect("state store");
        store.migrate().expect("migrate");

        // The run a crashed hosted session left behind: `run_init_with_output`
        // folds the forced rotation into the flag before the row is recorded.
        let mut first = request_from_json(r#"{"agent": "opencode"}"#)
            .into_init_args()
            .expect("valid request");
        first.rotate_keys = true;
        let recorded_run =
            super::super::resume::resolve_init_run(&first, &store).expect("record the first run");

        let resumed_args = request_from_json(r#"{"resume": true}"#)
            .into_init_args()
            .expect("valid request");
        let resumed_run = super::super::resume::resolve_init_run(&resumed_args, &store)
            .expect("hosted resume must adopt the recorded run");
        assert_eq!(resumed_run.id, recorded_run.id);

        // Rotation has no request field; it rides the recorded args, so a
        // hosted resume rotates exactly like a CLI `--resume` of the same run.
        let recorded = super::super::resume::recorded_init_args(&resumed_run).expect("recorded");
        assert!(recorded.rotate_keys);
        assert_eq!(recorded.agent.as_deref(), Some("opencode"));
    }

    #[test]
    fn start_init_request_maps_mcp_declarations_into_prompt_fields() {
        let args = request_from_json(
            r#"{
                "mcp_preset": ["linear"],
                "mcp_stdio": [
                    {"name": "files", "command": "mcp-files", "args": ["--root", "/data"], "env": ["FILES_TOKEN"]}
                ],
                "mcp_http": [
                    {"name": "search", "url": "https://mcp.example.com/mcp",
                     "headers": [{"name": "Authorization", "value_ref": "SEARCH_API_KEY"}]}
                ]
            }"#,
        )
        .into_init_args()
        .expect("valid request");
        assert_eq!(args.mcp_preset, vec!["linear".to_owned()]);
        assert!(args.mcp_stdio.is_empty());
        assert!(args.mcp_http.is_empty());
        assert_eq!(args.prompt_mcp_stdio.len(), 1);
        let stdio = &args.prompt_mcp_stdio[0];
        assert_eq!(stdio.name, "files");
        assert_eq!(stdio.command, "mcp-files");
        assert_eq!(stdio.args, vec!["--root".to_owned(), "/data".to_owned()]);
        assert_eq!(stdio.env, vec!["FILES_TOKEN".to_owned()]);
        assert_eq!(args.prompt_mcp_http.len(), 1);
        let http = &args.prompt_mcp_http[0];
        assert_eq!(http.name, "search");
        assert_eq!(http.url, "https://mcp.example.com/mcp");
        assert_eq!(http.headers.len(), 1);
        assert_eq!(http.headers[0].name, "Authorization");
        assert_eq!(http.headers[0].value_ref.as_deref(), Some("SEARCH_API_KEY"));
        assert_eq!(http.headers[0].value, None);
    }

    #[test]
    fn start_init_request_accepts_templated_header_and_env() {
        let args = request_from_json(
            r#"{
                "mcp_stdio": [
                    {"name": "db", "command": "db-mcp", "env": ["API_KEY", "URL=x-${DB_PASS}"]}
                ],
                "mcp_http": [
                    {"name": "relay", "url": "http://127.0.0.1:8787/mcp",
                     "headers": [{"name": "Authorization", "value": "Bearer ${RELAY_TOKEN}"}]}
                ]
            }"#,
        )
        .into_init_args()
        .expect("valid request");
        assert_eq!(
            args.prompt_mcp_stdio[0].env,
            vec!["API_KEY".to_owned(), "URL=x-${DB_PASS}".to_owned()]
        );
        let header = &args.prompt_mcp_http[0].headers[0];
        assert_eq!(header.value.as_deref(), Some("Bearer ${RELAY_TOKEN}"));
        assert_eq!(header.value_ref, None);
    }

    #[test]
    fn start_init_request_rejects_header_with_both_or_neither_value_source() {
        let both = request_from_json(
            r#"{"mcp_http": [{"name": "s", "url": "https://x.example/mcp",
                "headers": [{"name": "A", "value_ref": "R", "value": "${R}"}]}]}"#,
        )
        .into_init_args()
        .expect_err("both set must be rejected");
        assert!(both.to_string().contains("exactly one"), "{both}");

        let neither = request_from_json(
            r#"{"mcp_http": [{"name": "s", "url": "https://x.example/mcp",
                "headers": [{"name": "A"}]}]}"#,
        )
        .into_init_args()
        .expect_err("neither set must be rejected");
        assert!(neither.to_string().contains("exactly one"), "{neither}");
    }

    #[test]
    fn boundary_rejection_of_pasted_credentials_never_echoes_them() {
        let secret = "sk-live-AAAABBBBCCCC";
        let in_template = request_from_json(&format!(
            r#"{{"mcp_http": [{{"name": "s", "url": "https://x.example/mcp",
                "headers": [{{"name": "A", "value": "Bearer ${{{secret}}}"}}]}}]}}"#,
        ))
        .into_init_args()
        .expect_err("secret-shaped template ref must be rejected");
        assert!(!in_template.to_string().contains(secret), "{in_template}");

        let as_value_ref = request_from_json(&format!(
            r#"{{"mcp_http": [{{"name": "s", "url": "https://x.example/mcp",
                "headers": [{{"name": "A", "value_ref": "{secret}"}}]}}]}}"#,
        ))
        .into_init_args()
        .expect_err("secret-shaped value_ref must be rejected");
        assert!(!as_value_ref.to_string().contains(secret), "{as_value_ref}");

        let in_env = request_from_json(&format!(
            r#"{{"mcp_stdio": [{{"name": "db", "command": "db-mcp", "env": ["{secret}"]}}]}}"#,
        ))
        .into_init_args()
        .expect_err("secret-shaped env entry must be rejected");
        assert!(!in_env.to_string().contains(secret), "{in_env}");
    }

    #[test]
    fn start_init_request_rejects_malformed_templates_at_the_boundary() {
        let bad_header = request_from_json(
            r#"{"mcp_http": [{"name": "s", "url": "https://x.example/mcp",
                "headers": [{"name": "A", "value": "Bearer ${unclosed"}]}]}"#,
        )
        .into_init_args()
        .expect_err("unterminated template must be rejected");
        assert!(
            bad_header.to_string().contains("unterminated"),
            "{bad_header}"
        );

        let bad_env = request_from_json(
            r#"{"mcp_stdio": [{"name": "db", "command": "db-mcp", "env": ["URL=plaintext"]}]}"#,
        )
        .into_init_args()
        .expect_err("pure-literal env template must be rejected");
        assert!(
            bad_env.to_string().contains("no `${NAME}` reference"),
            "{bad_env}"
        );
    }

    #[test]
    fn start_init_request_maps_deps_and_flags() {
        let args = request_from_json(
            r#"{
                "deps": [{"name": "ripgrep", "shell": "apt-get install -y ripgrep"}],
                "deps_system": [{"name": "ffmpeg", "shell": "apt-get install -y ffmpeg"}],
                "deps_apply": true,
                "deps_apply_yes": true,
                "standard_agent_work_deps": true,
                "browser_use": true
            }"#,
        )
        .into_init_args()
        .expect("valid request");
        assert_eq!(
            args.dep,
            vec!["ripgrep=apt-get install -y ripgrep".to_owned()]
        );
        assert_eq!(
            args.dep_system,
            vec!["ffmpeg=apt-get install -y ffmpeg".to_owned()]
        );
        assert!(args.deps_apply);
        assert!(args.deps_apply_yes);
        assert!(args.standard_agent_work_deps);
        assert!(args.browser_use_profile);
    }

    #[test]
    fn start_init_request_maps_update_policies_into_args() {
        // Parity with the CLI's `--stack-update`/`--agent-update` flags: the
        // hosted contract must carry both update policies so a non-interactive
        // init can disable them, not just the interactive wizard.
        let args = request_from_json(
            r#"{
                "stack_update": "security",
                "stack_update_frequency": "2w",
                "agent_update": "off"
            }"#,
        )
        .into_init_args()
        .expect("valid request");
        assert_eq!(args.stack_update.as_deref(), Some("security"));
        assert_eq!(args.stack_update_frequency.as_deref(), Some("2w"));
        assert_eq!(args.agent_update.as_deref(), Some("off"));
        assert_eq!(args.agent_update_frequency, None);
    }

    #[test]
    fn start_init_request_rejects_frequency_without_policy() {
        // Mirrors clap's `requires`: a frequency with no policy is a 400 at the
        // boundary rather than a silently dropped field.
        let stack_error = request_from_json(r#"{"stack_update_frequency": "1w"}"#)
            .into_init_args()
            .expect_err("frequency without policy must be rejected");
        assert!(matches!(
            stack_error,
            StackError::InvalidParam {
                field: "stack_update_frequency",
                ..
            }
        ));
        let agent_error = request_from_json(r#"{"agent_update_frequency": "12h"}"#)
            .into_init_args()
            .expect_err("frequency without policy must be rejected");
        assert!(matches!(
            agent_error,
            StackError::InvalidParam {
                field: "agent_update_frequency",
                ..
            }
        ));
    }

    // Mirrors clap's `requires` on the provider family. Provider processing
    // returns early with no provider in hand, so each of these would otherwise
    // be accepted and then dropped without a word.
    #[test]
    fn start_init_request_rejects_provider_fields_without_their_anchor() {
        for (payload, offending) in [
            (r#"{"api_key_ref": "OPENROUTER_API_KEY"}"#, "api_key_ref"),
            (r#"{"custom_provider": true}"#, "custom_provider"),
            (r#"{"provider_name": "House LLM"}"#, "provider_name"),
            (
                r#"{"provider": "house", "base_url": "https://api.house.dev/v1"}"#,
                "base_url",
            ),
            (
                r#"{"provider": "house", "provider_api": "chat-completions"}"#,
                "provider_api",
            ),
            (
                r#"{"provider": "house", "model_name": "House 1"}"#,
                "model_name",
            ),
            (r#"{"provider": "house", "context": "200000"}"#, "context"),
            (
                r#"{"provider": "house", "output_max_tokens": "8192"}"#,
                "output_max_tokens",
            ),
        ] {
            let error = request_from_json(payload)
                .into_init_args()
                .expect_err("an unanchored provider field must be rejected");
            match error {
                StackError::InvalidParam { field, reason } => {
                    assert_eq!(field, offending, "payload {payload}");
                    assert!(
                        !reason.contains("house") && !reason.contains("House"),
                        "the rejection must not echo submitted values: {reason}"
                    );
                }
                other => panic!("expected an InvalidParam for {offending}, got {other:?}"),
            }
        }
    }

    #[test]
    fn start_init_request_accepts_a_fully_anchored_provider_family() {
        let plain =
            request_from_json(r#"{"provider": "openrouter", "api_key_ref": "OPENROUTER_API_KEY"}"#)
                .into_init_args()
                .expect("a provider with its key ref is a complete declaration");
        assert_eq!(plain.provider.as_deref(), Some("openrouter"));
        assert_eq!(plain.api_key_ref.as_deref(), Some("OPENROUTER_API_KEY"));

        let custom = request_from_json(
            r#"{
                "provider": "house",
                "custom_provider": true,
                "provider_name": "House LLM",
                "base_url": "https://api.house.dev/v1",
                "provider_api": "chat-completions",
                "model_name": "House 1",
                "context": "200000",
                "output_max_tokens": "8192"
            }"#,
        )
        .into_init_args()
        .expect("the whole custom-provider family is a complete declaration");
        assert!(custom.custom_provider);
        assert_eq!(custom.provider_name.as_deref(), Some("House LLM"));
        assert_eq!(custom.output_max_tokens.as_deref(), Some("8192"));
    }

    #[test]
    fn start_init_request_skills_declaration_clears_no_skills() {
        let args = request_from_json(
            r#"{"skills_source": "github:example", "skills": ["writing-plans"]}"#,
        )
        .into_init_args()
        .expect("valid request");
        assert!(!args.no_skills);
        assert_eq!(args.skills_source.as_deref(), Some("github:example"));
        assert_eq!(args.skills, vec!["writing-plans".to_owned()]);

        let essential = request_from_json(r#"{"essential_skills": true}"#)
            .into_init_args()
            .expect("valid request");
        assert!(!essential.no_skills);
        assert!(essential.essential_skills);

        let none = request_from_json(r#"{}"#)
            .into_init_args()
            .expect("valid request");
        assert!(none.no_skills);
    }

    #[test]
    fn start_init_request_maps_data_sources() {
        let args = request_from_json(
            r#"{
                "data_sources": [
                    {"type": "local", "path": "/srv/import"},
                    {"type": "https", "url": "https://example.com/data.tar.gz", "expected_sha256": "ab"},
                    {"type": "s3", "name": "corpus", "bucket": "my-bucket", "region": "us-east-1",
                     "prefix": "corpus/", "access_key_ref": "AWS_ACCESS_KEY_ID",
                     "secret_key_ref": "AWS_SECRET_ACCESS_KEY"}
                ]
            }"#,
        )
        .into_init_args()
        .expect("valid request");
        assert_eq!(args.prompt_data_sources.len(), 3);
        assert_eq!(args.prompt_data_sources[0].source_type, "local");
        assert_eq!(
            args.prompt_data_sources[0].path.as_deref(),
            Some("/srv/import")
        );
        assert_eq!(args.prompt_data_sources[1].source_type, "https");
        assert_eq!(
            args.prompt_data_sources[1].url.as_deref(),
            Some("https://example.com/data.tar.gz")
        );
        assert_eq!(
            args.prompt_data_sources[1].expected_sha256.as_deref(),
            Some("ab")
        );
        let s3 = &args.prompt_data_sources[2];
        assert_eq!(s3.source_type, "s3");
        assert_eq!(s3.name.as_deref(), Some("corpus"));
        assert_eq!(s3.bucket.as_deref(), Some("my-bucket"));
        assert_eq!(s3.region.as_deref(), Some("us-east-1"));
        assert_eq!(s3.prefix.as_deref(), Some("corpus/"));
        assert_eq!(s3.access_key_ref.as_deref(), Some("AWS_ACCESS_KEY_ID"));
        assert_eq!(s3.secret_key_ref.as_deref(), Some("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn start_init_request_rejects_invalid_environment_declarations() {
        assert!(
            serde_json::from_str::<StartInitRequest>(r#"{"mcp_servers": []}"#).is_err(),
            "unknown fields must be rejected"
        );
        assert!(
            serde_json::from_str::<StartInitRequest>(
                r#"{"data_sources": [{"type": "s3", "bucket": "b", "path": "/x", "access_key_ref": "A", "secret_key_ref": "S"}]}"#
            )
            .is_err(),
            "fields from another data-source type must be rejected"
        );
        assert!(
            serde_json::from_str::<StartInitRequest>(
                r#"{"data_sources": [{"type": "s3", "bucket": "b", "access_key_ref": "A", "secret_key_ref": "S"}]}"#
            )
            .is_err(),
            "s3 sources must declare a region"
        );
        for payload in [r#"{"deps_apply_yes": true}"#, r#"{"deps_apply": true}"#] {
            let mismatched_apply = request_from_json(payload).into_init_args();
            assert!(matches!(
                mismatched_apply,
                Err(StackError::InvalidParam {
                    field: "deps_apply",
                    ..
                })
            ));
        }
        for payload in [
            r#"{"skills_source": "github:example"}"#,
            r#"{"skills": ["writing-plans"]}"#,
        ] {
            let unpaired_skills = request_from_json(payload).into_init_args();
            assert!(matches!(
                unpaired_skills,
                Err(StackError::InvalidParam {
                    field: "skills",
                    ..
                })
            ));
        }
        let essential_conflict = request_from_json(
            r#"{"essential_skills": true, "skills_source": "github:example", "skills": ["x"]}"#,
        )
        .into_init_args();
        assert!(matches!(
            essential_conflict,
            Err(StackError::InvalidParam {
                field: "essential_skills",
                ..
            })
        ));
        let bad_dep_name =
            request_from_json(r#"{"deps": [{"name": "a=b", "shell": "true"}]}"#).into_init_args();
        assert!(matches!(
            bad_dep_name,
            Err(StackError::InvalidParam { field: "deps", .. })
        ));
        let empty_dep_shell =
            request_from_json(r#"{"deps": [{"name": "a", "shell": " "}]}"#).into_init_args();
        assert!(matches!(
            empty_dep_shell,
            Err(StackError::InvalidParam { field: "deps", .. })
        ));
    }

    #[test]
    fn start_init_request_declarations_assemble_into_starter_config() {
        let args = request_from_json(
            r#"{
                "mcp_stdio": [
                    {"name": "files", "command": "mcp-files", "args": ["--root", "/data"], "env": ["FILES_TOKEN"]}
                ],
                "mcp_http": [
                    {"name": "search", "url": "https://mcp.example.com/mcp",
                     "headers": [{"name": "Authorization", "value_ref": "SEARCH_API_KEY"}]}
                ],
                "deps": [{"name": "ripgrep", "shell": "apt-get install -y ripgrep"}],
                "data_sources": [
                    {"type": "s3", "bucket": "my-bucket", "region": "us-east-1",
                     "access_key_ref": "AWS_ACCESS_KEY_ID", "secret_key_ref": "AWS_SECRET_ACCESS_KEY"}
                ]
            }"#,
        )
        .into_init_args()
        .expect("valid request");
        let toml = super::super::starter_config::starter_config(&args)
            .expect("declarations must assemble into a starter config");
        for expected in [
            "name = \"files\"",
            "command = \"mcp-files\"",
            "FILES_TOKEN",
            "https://mcp.example.com/mcp",
            "SEARCH_API_KEY",
            "my-bucket",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(
                toml.contains(expected),
                "starter config must contain {expected}: {toml}"
            );
        }
    }

    #[test]
    fn hosted_driver_accepts_provider_password_and_model_responses() {
        let provider = send_select_response(
            HostedPromptKind::ProviderId,
            "provider for opencode",
            &["OpenRouter (openrouter)", "DeepSeek (deepseek)"],
            json!("OpenRouter (openrouter)"),
        );
        assert_eq!(provider, HostedPromptOutcome::Handled(Some(0)));

        let model = send_select_response(
            HostedPromptKind::Model,
            "select model",
            &["deepseek-v4-flash", "openai/gpt-5-mini"],
            json!({ "index": 1 }),
        );
        assert_eq!(model, HostedPromptOutcome::Handled(Some(1)));

        let session = test_session("init_driver_password");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            kind: HostedPromptKind::ProviderApiKeyValue,
            style: HostedPromptStyle::Password,
            prompt: "OPENROUTER_API_KEY".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
            inspection: None,
        };
        let handle = std::thread::spawn(move || driver.password(request));
        let pending = wait_for_pending_input(&session);
        session
            .submit_input(&pending.request_id, json!("sk-hosted-secret"))
            .expect("submit password");
        let password = handle.join().expect("driver thread").expect("password");
        assert_eq!(
            password,
            HostedPromptOutcome::Handled(Some("sk-hosted-secret".to_owned()))
        );
        let events = serde_json::to_string(&session.events_after(0)).expect("events");
        assert!(!events.contains("sk-hosted-secret"));
    }

    #[test]
    fn hosted_driver_streams_testflight_confirmation() {
        let session = test_session("init_driver_testflight");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            kind: HostedPromptKind::TestflightConfirm,
            style: HostedPromptStyle::Confirm,
            prompt: "run testflight now?".to_owned(),
            required: true,
            default: Some(false),
            items: Vec::new(),
            inspection: None,
        };
        let handle = std::thread::spawn(move || driver.confirm(request));
        let pending = wait_for_pending_input(&session);
        assert_eq!(pending.prompt, "run testflight now?");
        session
            .submit_input(&pending.request_id, json!(true))
            .expect("submit confirm");
        let confirm = handle.join().expect("driver thread").expect("confirm");
        assert_eq!(confirm, HostedPromptOutcome::Handled(true));
    }

    #[test]
    fn hosted_driver_streams_redacted_native_config_review() {
        let inspected = crate::runtime::agent::native_config_import::inspect_native_config(
            "opencode",
            Some("opencode.json"),
            r#"{"theme":"raw-secret-value","model":"openai/gpt-5"}"#,
        )
        .expect("inspect");
        let inspection = inspected.inspection().clone();
        let revision = inspection.revision.clone();
        let session = test_session("init_driver_native_config");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            kind: HostedPromptKind::NativeConfigReview,
            style: HostedPromptStyle::NativeConfigReview,
            prompt: "Review native Agent config".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
            inspection: Some(inspection),
        };
        let handle = std::thread::spawn(move || driver.native_config_review(request));
        let pending = wait_for_pending_input(&session);
        assert_eq!(pending.kind, "native_config_review");
        assert_eq!(pending.style, "native_config_review");
        assert_eq!(
            pending.inspection.as_ref().expect("inspection").revision,
            revision
        );
        let serialized = serde_json::to_string(&pending).expect("serialize");
        assert!(!serialized.contains("raw-secret-value"));
        session
            .submit_input(
                &pending.request_id,
                json!({
                    "revision": revision,
                    "selected_managed_field_ids": ["provider", "model"],
                    "executable_settings_acknowledged": false
                }),
            )
            .expect("submit review");
        let selection = handle.join().expect("driver thread").expect("review");
        assert!(matches!(selection, HostedPromptOutcome::Handled(_)));
        let events = serde_json::to_string(&session.events_after(0)).expect("events");
        assert!(!events.contains("raw-secret-value"));
    }

    #[test]
    fn hosted_driver_leaves_non_bootstrap_text_prompts_unhandled() {
        let session = test_session("init_driver_text");
        let driver = SessionPromptDriver { session };
        let request = HostedPromptRequest {
            kind: HostedPromptKind::ConfigSourcePath,
            style: HostedPromptStyle::Text,
            prompt: "acps-config.toml path".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
            inspection: None,
        };
        let outcome = driver.text(request).expect("text");
        assert_eq!(outcome, HostedPromptOutcome::Unhandled);
    }

    /// The five prompt strings the update-policy flow emits. The Text entries
    /// are the custom-frequency input behind the select's Custom branch
    /// (prompts.rs renders them from the consumer's DurationLimits: stack =
    /// day/week min 1 day, agent = hour/day/week min 1 hour).
    const UPDATE_POLICY_PROMPTS: [(HostedPromptKind, HostedPromptStyle, &str); 5] = [
        (
            HostedPromptKind::StackUpdatePolicy,
            HostedPromptStyle::Select,
            "acp-stack auto-update",
        ),
        (
            HostedPromptKind::UpdateFrequency,
            HostedPromptStyle::Select,
            "update frequency",
        ),
        (
            HostedPromptKind::AgentUpdateEnabled,
            HostedPromptStyle::Confirm,
            "Auto-update this agent's harness?",
        ),
        (
            HostedPromptKind::UpdateFrequencyCustom,
            HostedPromptStyle::Text,
            "frequency (e.g. 1d, 3w; minimum 1 day)",
        ),
        (
            HostedPromptKind::UpdateFrequencyCustom,
            HostedPromptStyle::Text,
            "frequency (e.g. 1h, 3w; minimum 1 hour)",
        ),
    ];

    #[test]
    fn hosted_driver_never_streams_update_policy_prompts() {
        // The api.md/init.md contract promises the stack- and agent-update
        // prompts stay out of the streamed set — hosted clients supply these
        // up front via `stack_update`/`agent_update`.
        for (kind, style, text) in UPDATE_POLICY_PROMPTS {
            let request = hosted_test_request(kind, style, text, &[]);
            assert!(
                !should_handle_hosted_prompt(&request),
                "update-policy prompt `{text}` must not be streamed to hosted clients"
            );
        }
    }

    #[test]
    fn hosted_prompt_allow_list_keys_off_kind_not_prompt_text() {
        // The same wording under a hostable kind streams, which is what proves
        // the decision moved off string matching: rewording a prompt can no
        // longer change hostability, and only the kind can.
        for (_, style, text) in UPDATE_POLICY_PROMPTS {
            let request = hosted_test_request(HostedPromptKind::Model, style, text, &[]);
            assert!(
                should_handle_hosted_prompt(&request),
                "prompt `{text}` must stream once carried by a hostable kind"
            );
        }
    }

    #[test]
    fn hostable_kinds_carry_their_wire_kind_into_input_required_and_pending_input() {
        for (kind, style) in [
            (HostedPromptKind::Agent, HostedPromptStyle::SearchableSelect),
            (
                HostedPromptKind::ProviderId,
                HostedPromptStyle::SearchableSelect,
            ),
            (HostedPromptKind::Model, HostedPromptStyle::SearchableSelect),
            (HostedPromptKind::Mode, HostedPromptStyle::SearchableSelect),
            (HostedPromptKind::McpTransport, HostedPromptStyle::Select),
            (HostedPromptKind::McpRowAction, HostedPromptStyle::Select),
            (HostedPromptKind::McpAdd, HostedPromptStyle::Confirm),
            (
                HostedPromptKind::CustomProviderConfirm,
                HostedPromptStyle::Confirm,
            ),
            (
                HostedPromptKind::TestflightConfirm,
                HostedPromptStyle::Confirm,
            ),
            (HostedPromptKind::ProviderName, HostedPromptStyle::Text),
            (HostedPromptKind::BaseUrl, HostedPromptStyle::Text),
            (HostedPromptKind::ApiKeyRef, HostedPromptStyle::Text),
            (HostedPromptKind::McpStdioName, HostedPromptStyle::Text),
            (HostedPromptKind::McpStdioCommand, HostedPromptStyle::Text),
            (HostedPromptKind::McpStdioArgs, HostedPromptStyle::Text),
            (HostedPromptKind::McpStdioEnvRefs, HostedPromptStyle::Text),
            (HostedPromptKind::McpHttpName, HostedPromptStyle::Text),
            (HostedPromptKind::McpHttpUrl, HostedPromptStyle::Text),
            (HostedPromptKind::McpHttpHeaders, HostedPromptStyle::Text),
            (
                HostedPromptKind::ProviderApiKeyValue,
                HostedPromptStyle::Password,
            ),
            (
                HostedPromptKind::SecretRefValue,
                HostedPromptStyle::Password,
            ),
        ] {
            let request = hosted_test_request(kind, style, "prompt", &["alpha", "beta"]);
            assert!(
                should_handle_hosted_prompt(&request),
                "kind `{}` must be streamed to hosted clients",
                kind.as_str()
            );

            let session = test_session("init_kind_wire");
            let driver = SessionPromptDriver {
                session: session.clone(),
            };
            let handle = std::thread::spawn(move || driver.select(request));
            let pending = wait_for_pending_input(&session);
            assert_eq!(pending.kind, kind.as_str());
            let frame = recorded_frame(&session, 2);
            assert!(
                frame.contains(&format!(r#""kind":"{}""#, kind.as_str())),
                "input_required frame must carry the kind: {frame}"
            );
            session
                .submit_input(&pending.request_id, Value::Null)
                .expect("submit input");
            handle.join().expect("driver thread").expect("select");
        }
    }

    #[test]
    fn select_options_carry_stable_values_distinct_from_labels() {
        let session = test_session("init_option_values");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = hosted_test_request(
            HostedPromptKind::Model,
            HostedPromptStyle::SearchableSelect,
            "select model",
            &["alpha", "beta"],
        );
        let handle = std::thread::spawn(move || driver.select(request));
        let pending = wait_for_pending_input(&session);
        let values: Vec<_> = pending
            .options
            .iter()
            .map(|option| option.value.as_str())
            .collect();
        assert_eq!(values, ["id_alpha", "id_beta"]);
        for option in &pending.options {
            assert_ne!(option.value, option.label);
        }
        session
            .submit_input(&pending.request_id, Value::Null)
            .expect("submit input");
        handle.join().expect("driver thread").expect("select");
    }

    #[test]
    fn select_answers_resolve_by_value_index_label_or_null() {
        let labels = ["alpha", "beta"];
        let by_value = send_select_response(
            HostedPromptKind::Model,
            "select model",
            &labels,
            json!({"value": "id_beta"}),
        );
        assert_eq!(by_value, HostedPromptOutcome::Handled(Some(1)));

        let by_index = send_select_response(
            HostedPromptKind::Model,
            "select model",
            &labels,
            json!({"index": 0}),
        );
        assert_eq!(by_index, HostedPromptOutcome::Handled(Some(0)));

        let by_label = send_select_response(
            HostedPromptKind::Model,
            "select model",
            &labels,
            json!("beta"),
        );
        assert_eq!(by_label, HostedPromptOutcome::Handled(Some(1)));

        let skipped = send_select_response(
            HostedPromptKind::Model,
            "select model",
            &labels,
            Value::Null,
        );
        assert_eq!(skipped, HostedPromptOutcome::Handled(None));
    }

    #[test]
    fn unknown_select_value_is_rejected_as_invalid_param() {
        let error = select_result(
            HostedPromptKind::Model,
            "select model",
            &["alpha", "beta"],
            json!({"value": "id_gamma"}),
        )
        .expect_err("unknown option value must be rejected");
        assert!(matches!(
            error,
            StackError::InvalidParam { field: "init", .. }
        ));
        // Bare strings match labels only, so an id sent that way is unknown too.
        let error = select_result(
            HostedPromptKind::Model,
            "select model",
            &["alpha", "beta"],
            json!("id_alpha"),
        )
        .expect_err("an option id is not a label");
        assert!(matches!(
            error,
            StackError::InvalidParam { field: "init", .. }
        ));
    }

    /// Answers a streamed prompt sequence one request at a time. Remembering
    /// the last request id is what keeps the poller from re-reading a prompt
    /// it already answered, before the wizard thread wakes and clears it.
    struct HostedPromptTranscript {
        session: Arc<HostedInitSession>,
        last_request_id: Option<String>,
    }

    impl HostedPromptTranscript {
        fn new(session: Arc<HostedInitSession>) -> Self {
            Self {
                session,
                last_request_id: None,
            }
        }

        fn next_pending(&self) -> PublicInputRequest {
            for _ in 0..200 {
                if let Some(input) = lock_unpoisoned(&self.session.inner).pending_input.clone()
                    && self.last_request_id.as_ref() != Some(&input.request_id)
                {
                    return input;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting for the next hosted init input request");
        }

        fn answer(&mut self, kind: HostedPromptKind, response: Value) -> PublicInputRequest {
            let pending = self.next_pending();
            assert_eq!(
                pending.kind,
                kind.as_str(),
                "unexpected prompt on the stream: {pending:?}"
            );
            self.session
                .submit_input(&pending.request_id, response)
                .expect("submit input");
            self.last_request_id = Some(pending.request_id.clone());
            pending
        }
    }

    fn option_values(request: &PublicInputRequest) -> Vec<&str> {
        request
            .options
            .iter()
            .map(|option| option.value.as_str())
            .collect()
    }

    /// A probe advertisement carrying the given `mcpCapabilities`.
    fn mcp_capabilities(
        advertised: Value,
    ) -> crate::runtime::agent::acp_bridge::AgentCapabilitiesDto {
        serde_json::from_value(json!({
            "protocol_version": 1,
            "capabilities": { "mcpCapabilities": advertised },
            "agent_name": "placebo",
            "agent_title": null,
            "agent_version": null,
        }))
        .expect("capabilities fixture")
    }

    /// The `offer_http` the `mcp_configure` step computes, derived from a probe
    /// fixture rather than a bare bool so the picker stays tied to the real
    /// capability accessor.
    fn offer_http_for(advertised: Value) -> bool {
        mcp_capabilities(advertised).supports_mcp_capability("http")
    }

    /// Runs the post-probe MCP step's prompt half against a hosted session, in
    /// the order `mcp_configure` drives it: the add confirmation, then the
    /// transport wizard.
    fn hosted_mcp_wizard(
        session: Arc<HostedInitSession>,
        offer_http: bool,
    ) -> std::thread::JoinHandle<Result<InitArgs>> {
        let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver { session });
        std::thread::spawn(move || {
            prompt::with_hosted_driver(driver, || {
                let mut args = request_from_json(r#"{"agent":"placebo"}"#)
                    .into_init_args()
                    .expect("valid request");
                if prompt::confirm(HostedPromptKind::McpAdd, true, "Add MCP servers?", false)? {
                    prompt_mcp_servers(true, &mut args, offer_http)?;
                }
                Ok(args)
            })
        })
    }

    // The lifted exclusion, end to end: every MCP prompt reaches the client
    // with its kind, selects address their rows by stable id, and the answers
    // land as declared servers.
    #[test]
    fn hosted_mcp_wizard_streams_the_add_sequence_with_kinds_and_stable_values() {
        let session = test_session("init_mcp_wizard");
        let wizard = hosted_mcp_wizard(session.clone(), offer_http_for(json!({"http": true})));
        let mut transcript = HostedPromptTranscript::new(session.clone());

        transcript.answer(HostedPromptKind::McpAdd, json!(true));
        let transport =
            transcript.answer(HostedPromptKind::McpTransport, json!({"value": "stdio"}));
        assert_eq!(option_values(&transport), ["stdio", "http", "__done"]);
        transcript.answer(HostedPromptKind::McpStdioName, json!("files"));
        transcript.answer(HostedPromptKind::McpStdioCommand, json!("mcp-files"));
        transcript.answer(HostedPromptKind::McpStdioArgs, json!("--root, /data"));
        transcript.answer(HostedPromptKind::McpStdioEnvRefs, json!("FILES_TOKEN"));
        let row_action =
            transcript.answer(HostedPromptKind::McpRowAction, json!({"value": "done"}));
        assert_eq!(
            option_values(&row_action),
            ["add_another", "discard", "done"]
        );

        transcript.answer(HostedPromptKind::McpTransport, json!({"value": "http"}));
        transcript.answer(HostedPromptKind::McpHttpName, json!("search"));
        transcript.answer(
            HostedPromptKind::McpHttpUrl,
            json!("https://mcp.example.com/mcp"),
        );
        transcript.answer(
            HostedPromptKind::McpHttpHeaders,
            json!("Authorization:SEARCH_API_KEY"),
        );
        transcript.answer(HostedPromptKind::McpRowAction, json!({"value": "done"}));
        transcript.answer(HostedPromptKind::McpTransport, json!({"value": "__done"}));

        let args = wizard.join().expect("wizard thread").expect("mcp wizard");
        assert_eq!(args.prompt_mcp_stdio.len(), 1);
        let stdio = &args.prompt_mcp_stdio[0];
        assert_eq!(stdio.name, "files");
        assert_eq!(stdio.command, "mcp-files");
        assert_eq!(stdio.args, ["--root".to_owned(), "/data".to_owned()]);
        assert_eq!(stdio.env, ["FILES_TOKEN".to_owned()]);
        assert_eq!(args.prompt_mcp_http.len(), 1);
        let http = &args.prompt_mcp_http[0];
        assert_eq!(http.name, "search");
        assert_eq!(http.url, "https://mcp.example.com/mcp");
        assert_eq!(http.headers.len(), 1);
        assert_eq!(http.headers[0].name, "Authorization");
        assert_eq!(http.headers[0].value_ref.as_deref(), Some("SEARCH_API_KEY"));

        let events = serde_json::to_string(&session.events_after(0)).expect("events");
        for kind in [
            "mcp_add",
            "mcp_transport",
            "mcp_stdio_name",
            "mcp_http_headers",
        ] {
            assert!(
                events.contains(&format!(r#""kind":"{kind}""#)),
                "the streamed frames must carry `{kind}`"
            );
        }
    }

    // Capability gating survives the lift: the transport the agent never
    // advertised is not offered to a hosted client either.
    #[test]
    fn hosted_mcp_transport_options_follow_the_probed_capabilities() {
        let session = test_session("init_mcp_transport_options");
        let wizard = hosted_mcp_wizard(session.clone(), offer_http_for(json!({})));
        let mut transcript = HostedPromptTranscript::new(session.clone());

        transcript.answer(HostedPromptKind::McpAdd, json!(true));
        let transport =
            transcript.answer(HostedPromptKind::McpTransport, json!({"value": "__done"}));
        assert_eq!(option_values(&transport), ["stdio", "__done"]);

        let args = wizard.join().expect("wizard thread").expect("mcp wizard");
        assert!(args.prompt_mcp_stdio.is_empty());
        assert!(args.prompt_mcp_http.is_empty());
    }

    // Refs travel as text; a client that pastes the credential itself is
    // rejected by the boundary screening, and neither the error nor the
    // stream repeats what it pasted.
    #[test]
    fn a_pasted_credential_in_a_header_ref_is_rejected_without_echoing_it() {
        const PASTED: &str = "sk-live-hosted-mcp-header-value";
        // The four shapes a paste takes: dropped into the ref position of an
        // otherwise well-formed entry, pasted whole where `HEADER:SECRET_REF`
        // was asked for, and pasted into the header position of an entry that
        // does split, with and without a scheme token in front. The last one is
        // why the header-name error names no input at all: the screening
        // heuristic matches credential prefixes, so `Bearer sk-...` slips past
        // it, and only a reason that quotes nothing keeps the paste out of the
        // terminal error frame, the reconnect hello, and replayable history.
        // `screened` marks the forms the heuristic itself catches.
        for (index, entry, screened) in [
            (0, format!("Authorization:{PASTED}"), true),
            (1, PASTED.to_owned(), true),
            (2, format!("{PASTED} extra:LINEAR_API_KEY"), true),
            (3, format!("Bearer {PASTED}:LINEAR_API_KEY"), false),
        ] {
            let session = test_session(&format!("init_mcp_header_screen_{index}"));
            let wizard = hosted_mcp_wizard(session.clone(), true);
            let mut transcript = HostedPromptTranscript::new(session.clone());

            transcript.answer(HostedPromptKind::McpAdd, json!(true));
            transcript.answer(HostedPromptKind::McpTransport, json!({"value": "http"}));
            transcript.answer(HostedPromptKind::McpHttpName, json!("search"));
            transcript.answer(
                HostedPromptKind::McpHttpUrl,
                json!("https://mcp.example.com/mcp"),
            );
            transcript.answer(HostedPromptKind::McpHttpHeaders, json!(entry));

            let error = wizard
                .join()
                .expect("wizard thread")
                .expect_err("a pasted credential must be rejected");
            if screened {
                assert!(
                    matches!(error, StackError::SecretRefLooksLikeValue { .. }),
                    "`{entry}` was not screened: {error:?}"
                );
            } else {
                // Pinned so the case cannot pass by failing somewhere earlier:
                // this form reaches the header-name check specifically.
                assert!(
                    error
                        .public_message()
                        .contains("not a valid HTTP header name"),
                    "`{entry}` must be rejected by the header-name check: {error:?}"
                );
            }
            assert!(!error.to_string().contains(PASTED));
            assert!(!error.public_message().contains(PASTED));
            // The rejection travels as a session failure, so the surfaces it
            // reaches are asserted through the same path a client sees.
            session.set_error(error.error_code(), error.public_message());
            let events = serde_json::to_string(&session.events_after(0)).expect("events");
            let status = serde_json::to_string(&session.status_snapshot()).expect("status");
            for surface in [events, session.hello_frame(), status] {
                assert!(
                    !surface.contains(PASTED),
                    "a pasted credential must never be echoed onto {surface}"
                );
            }
        }
    }

    // The env-ref prompt takes bare ref names, so the same paste lands on the
    // name-shape check instead of the header parser. A dashed token matches
    // none of the screening heuristic's prefixes, which is exactly why that
    // check may not quote the entry back.
    #[test]
    fn a_pasted_credential_in_a_stdio_env_ref_is_rejected_without_echoing_it() {
        const PASTED: &str = "xai-9f2c8b1a-4d7e-11ef-9a3b-0242ac120002";
        let session = test_session("init_mcp_env_ref_screen");
        let wizard = hosted_mcp_wizard(session.clone(), true);
        let mut transcript = HostedPromptTranscript::new(session.clone());

        transcript.answer(HostedPromptKind::McpAdd, json!(true));
        transcript.answer(HostedPromptKind::McpTransport, json!({"value": "stdio"}));
        transcript.answer(HostedPromptKind::McpStdioName, json!("files"));
        transcript.answer(HostedPromptKind::McpStdioCommand, json!("mcp-files"));
        transcript.answer(HostedPromptKind::McpStdioArgs, json!(""));
        transcript.answer(HostedPromptKind::McpStdioEnvRefs, json!(PASTED));

        let error = wizard
            .join()
            .expect("wizard thread")
            .expect_err("a pasted credential must be rejected");
        // Pinned so the case cannot pass by failing earlier: the screening
        // heuristic does not recognize this shape, so the name-shape check is
        // the one that has to reject it without an echo.
        assert!(
            error.public_message().contains("secret ref name must use"),
            "`{PASTED}` must be rejected by the ref-name check: {error:?}"
        );
        assert!(!error.to_string().contains(PASTED));
        assert!(!error.public_message().contains(PASTED));
        session.set_error(error.error_code(), error.public_message());
        let events = serde_json::to_string(&session.events_after(0)).expect("events");
        let status = serde_json::to_string(&session.status_snapshot()).expect("status");
        for surface in [events, session.hello_frame(), status] {
            assert!(
                !surface.contains(PASTED),
                "a pasted credential must never be echoed onto {surface}"
            );
        }
    }

    // The values behind those refs take the password lane, and the collected
    // secret reaches the store without ever appearing in the event history.
    #[test]
    fn hosted_mcp_secret_values_are_collected_as_password_prompts() {
        const SECRET: &str = "files-token-value";
        let home = tempfile::tempdir().expect("tempdir");
        let session = test_session("init_mcp_secret_refs");
        let driver: Arc<dyn HostedPromptDriver> = Arc::new(SessionPromptDriver {
            session: session.clone(),
        });
        let home_path = home.path().to_path_buf();
        let collector = std::thread::spawn(move || {
            let mut store = SecretStore::open_or_create(&home_path).expect("secret store");
            let stored = prompt::with_hosted_driver(driver, || {
                collect_mcp_secret_refs_for_init(true, &config_with_mcp_env_ref(), &mut store)
            })?;
            Ok::<_, StackError>((stored, store))
        });

        let mut transcript = HostedPromptTranscript::new(session.clone());
        let pending = transcript.answer(HostedPromptKind::SecretRefValue, json!(SECRET));
        assert_eq!(pending.style, "password");
        assert_eq!(pending.prompt, "FILES_TOKEN");

        let (stored, store) = collector
            .join()
            .expect("collector thread")
            .expect("collect");
        assert_eq!(stored, ["FILES_TOKEN".to_owned()]);
        assert_eq!(store.get("FILES_TOKEN").expect("stored ref"), SECRET);
        let events = serde_json::to_string(&session.events_after(0)).expect("events");
        assert!(
            !events.contains(SECRET),
            "a collected secret value must never reach the stream"
        );
    }

    fn config_with_mcp_env_ref() -> config::Config {
        let mut config = config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture config");
        config.mcp.servers = mcp_servers_from_prompted(
            &[InitMcpStdioServer {
                name: "files".to_owned(),
                command: "mcp-files".to_owned(),
                args: Vec::new(),
                env: vec!["FILES_TOKEN".to_owned()],
            }],
            &[],
        )
        .expect("declared servers");
        config
    }

    #[test]
    fn stale_input_request_id_is_rejected() {
        let session = test_session("init_stale_input");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            kind: HostedPromptKind::ProviderApiKeyValue,
            style: HostedPromptStyle::Password,
            prompt: "OPENROUTER_API_KEY".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
            inspection: None,
        };
        let handle = std::thread::spawn(move || driver.password(request));
        let pending = wait_for_pending_input(&session);

        let stale_frame = json!({
            "type": "input",
            "request_id": "stale_request",
            "value": "sk-hosted-secret"
        })
        .to_string();
        match handle_client_frame(&session, &stale_frame) {
            ClientFrameOutcome::Send(frame) => {
                let value: Value = serde_json::from_str(&frame).expect("error frame");
                assert_eq!(value["type"], "error");
                assert_eq!(value["code"], "init.input_rejected");
            }
            _ => panic!("stale input should be rejected with an error frame"),
        }

        session
            .submit_input(&pending.request_id, json!("sk-hosted-secret"))
            .expect("submit correct input");
        let password = handle.join().expect("driver thread").expect("password");
        assert_eq!(
            password,
            HostedPromptOutcome::Handled(Some("sk-hosted-secret".to_owned()))
        );
    }

    #[test]
    fn result_is_replay_only_and_ack_is_terminal() {
        let session = test_session("init_result");
        session.set_result(json!({
            "status": "initialized",
            "session_key": "acps_session_secret",
            "admin_key": "acps_admin_secret"
        }));

        let snapshot = serde_json::to_string(&session.status_snapshot()).expect("snapshot");
        assert!(snapshot.contains("completed_awaiting_ack"));
        assert!(!snapshot.contains("acps_session_secret"));
        assert!(!snapshot.contains("acps_admin_secret"));

        let replay = match handle_client_frame(&session, r#"{"type":"replay_result"}"#) {
            ClientFrameOutcome::Send(frame) => frame,
            _ => panic!("replay_result should return a result frame"),
        };
        assert!(replay.contains("acps_session_secret"));
        assert!(replay.contains("acps_admin_secret"));

        match handle_client_frame(&session, r#"{"type":"ack_result"}"#) {
            ClientFrameOutcome::Close(frame) => {
                let value: Value = serde_json::from_str(&frame).expect("ack frame");
                assert_eq!(value["type"], "ack_accepted");
            }
            _ => panic!("ack_result should close the session"),
        }

        assert_eq!(session.status(), "closed");
        assert!(session.result_frame().is_none());
        assert!(!session.is_active());
    }

    #[test]
    fn cancel_prevents_late_result_publication() {
        let session = test_session("init_cancel");
        session.cancel("backend_cancel");
        session.set_result(json!({
            "status": "initialized",
            "session_key": "acps_session_after_cancel",
            "admin_key": "acps_admin_after_cancel"
        }));
        session.set_error("init.failed", "should not replace cancel".to_owned());

        assert_eq!(session.status(), "canceled");
        assert!(session.result_frame().is_none());
        let snapshot = serde_json::to_string(&session.status_snapshot()).expect("snapshot");
        assert!(!snapshot.contains("acps_session_after_cancel"));
        assert!(!snapshot.contains("should not replace cancel"));
    }

    #[tokio::test]
    async fn error_is_parked_until_acked() {
        let manager = HostedInitManager::new();
        let session = HostedInitSession::new("init_error".to_owned(), manager.shutdown.clone());
        *lock_unpoisoned(&manager.active) = Some(session.clone());

        {
            let waiter = manager.wait_for_terminal();
            tokio::pin!(waiter);
            session.set_error("init.failed", "provider setup failed".to_owned());

            // The failure parks: the process must stay up so the backend can
            // replay and acknowledge the typed error.
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                    .await
                    .is_err(),
                "set_error must not notify the terminal waiter"
            );
            assert_eq!(session.status(), "errored");
            assert!(session.is_active());
            assert!(session.unacked_error_age().is_some());

            // A racing backend cancel must not overwrite the typed failure.
            session.cancel("backend_cancel");
            assert_eq!(session.status(), "errored");

            let replay = match handle_client_frame(&session, r#"{"type":"replay_error"}"#) {
                ClientFrameOutcome::Send(frame) => frame,
                _ => panic!("replay_error should return an error frame"),
            };
            let value: Value = serde_json::from_str(&replay).expect("error frame");
            assert_eq!(value["type"], "error");
            assert_eq!(value["code"], "init.failed");
            assert_eq!(value["message"], "provider setup failed");

            match handle_client_frame(&session, r#"{"type":"ack_error"}"#) {
                ClientFrameOutcome::Close(frame) => {
                    let value: Value = serde_json::from_str(&frame).expect("ack frame");
                    assert_eq!(value["type"], "error_acked");
                }
                _ => panic!("ack_error should close the session"),
            }
            tokio::time::timeout(Duration::from_secs(1), &mut waiter)
                .await
                .expect("terminal waiter should be notified after ack_error");
        }
        assert_eq!(session.status(), "errored");
        assert!(!session.is_active());
        assert!(session.unacked_error_age().is_none());
        let error = manager
            .terminal_result()
            .expect_err("errored session should return failure");
        assert!(
            error
                .public_message()
                .contains("init.failed: provider setup failed")
        );
    }

    #[tokio::test]
    async fn ack_error_is_rejected_without_parked_error() {
        let session = test_session("init_no_error");
        match handle_client_frame(&session, r#"{"type":"ack_error"}"#) {
            ClientFrameOutcome::Send(frame) => {
                let value: Value = serde_json::from_str(&frame).expect("error frame");
                assert_eq!(value["code"], "init.ack_rejected");
            }
            _ => panic!("ack_error without a parked error must be rejected"),
        }
        match handle_client_frame(&session, r#"{"type":"replay_error"}"#) {
            ClientFrameOutcome::Send(frame) => {
                let value: Value = serde_json::from_str(&frame).expect("error frame");
                assert_eq!(value["code"], "init.error_unavailable");
            }
            _ => panic!("replay_error without a recorded error must be rejected"),
        }
    }

    #[tokio::test]
    async fn parked_error_blocks_new_session_and_surfaces_in_status() {
        let session = test_session("init_error_409");
        session.set_error("init.failed", "provider setup failed".to_owned());
        let app = app_with_session(session);

        let (status, _) = request_json(
            app.clone(),
            Method::POST,
            "/v1/init/sessions",
            Some(json!({})),
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, body) = request_json(
            app,
            Method::GET,
            "/v1/init/sessions/init_error_409",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["status"], "errored");
        assert_eq!(body["data"]["error"]["code"], "init.failed");
    }

    #[tokio::test]
    async fn expiring_unacked_error_notifies_shutdown_and_keeps_status() {
        let manager = HostedInitManager::new();
        let session = HostedInitSession::new("init_error_exp".to_owned(), manager.shutdown.clone());
        *lock_unpoisoned(&manager.active) = Some(session.clone());
        session.set_error("init.failed", "provider setup failed".to_owned());

        let waiter = manager.wait_for_terminal();
        tokio::pin!(waiter);
        session.expire("error_ack_timeout");
        tokio::time::timeout(Duration::from_secs(1), &mut waiter)
            .await
            .expect("expiring an unacked error must notify shutdown");
        assert_eq!(session.status(), "errored");
        assert!(
            manager.terminal_result().is_err(),
            "expired failure must still exit non-zero"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn errored_session_expires_after_ack_grace_with_connected_ws() {
        let manager = HostedInitManager::new();
        let session = HostedInitSession::new("init_error_ws".to_owned(), manager.shutdown.clone());
        *lock_unpoisoned(&manager.active) = Some(session.clone());
        // A held socket must not defer the grace: the check ignores
        // connection state, unlike the idle clock.
        session.ws_connected();
        session.set_error("init.failed", "provider setup failed".to_owned());

        // Idle timeout disabled; only the error-ack grace can fire.
        let reaper = tokio::spawn(reap_idle_session(manager.clone(), None));
        tokio::time::sleep(ERROR_ACK_GRACE + IDLE_REAPER_TICK * 2).await;
        tokio::time::timeout(Duration::from_secs(1), reaper)
            .await
            .expect("reaper should stop after expiring the error")
            .expect("reaper task");
        assert_eq!(session.status(), "errored");
        assert!(!session.is_active());
    }

    fn app_with_manager(manager: Arc<HostedInitManager>) -> Router {
        build_bootstrap_router(
            BootstrapState {
                token: Arc::new(TEST_TOKEN.to_owned()),
                allowed_origins: Arc::new(vec!["https://backend.example".to_owned()]),
                manager,
                native_config_mutation: Arc::new(TokioMutex::new(())),
            },
            super::super::STARTER_MAX_REQUEST_BYTES,
        )
    }

    fn app_with_session(session: Arc<HostedInitSession>) -> Router {
        let manager = HostedInitManager::new();
        *lock_unpoisoned(&manager.active) = Some(session);
        app_with_manager(manager)
    }

    async fn request_json(
        app: Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        };
        let response = app
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("json body")
        };
        (status, value)
    }

    async fn request_raw_json(
        app: Router,
        method: Method,
        uri: &str,
        body: &'static str,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = app
            .oneshot(builder.body(Body::from(body)).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let value = serde_json::from_slice(&bytes).expect("json body");
        (status, value)
    }

    #[tokio::test]
    async fn bootstrap_api_auth_conflict_status_and_event_replay_are_non_secret() {
        let session = test_session("init_api");
        session.push_event(ServerEvent::Progress {
            message: "first".to_owned(),
        });
        session.push_event(ServerEvent::Progress {
            message: "second".to_owned(),
        });
        session.set_result(json!({
            "status": "initialized",
            "session_key": "acps_session_api_secret",
            "admin_key": "acps_admin_api_secret"
        }));
        let app = app_with_session(session);

        let (status, _) = request_json(
            app.clone(),
            Method::GET,
            "/v1/init/sessions/init_api",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) = request_json(
            app.clone(),
            Method::GET,
            "/v1/init/sessions/init_api",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["status"], "completed_awaiting_ack");
        assert_eq!(body["data"]["result_available"], true);
        let status_body = body.to_string();
        assert!(!status_body.contains("acps_session_api_secret"));
        assert!(!status_body.contains("acps_admin_api_secret"));

        let (status, _) = request_json(
            app.clone(),
            Method::POST,
            "/v1/init/sessions",
            Some(json!({})),
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, body) = request_json(
            app,
            Method::GET,
            "/v1/init/sessions/init_api/events?after_seq=1",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let events_body = body.to_string();
        assert!(events_body.contains("second"));
        assert!(events_body.contains("result_ready"));
        assert!(!events_body.contains("acps_session_api_secret"));
        assert!(!events_body.contains("acps_admin_api_secret"));
    }

    #[tokio::test]
    async fn bootstrap_api_rejects_duplicate_authorization_headers() {
        let app = app_with_session(test_session("init_duplicate_auth"));
        let request = Request::builder()
            .method(Method::GET)
            .uri("/v1/init/sessions/init_duplicate_auth")
            .header(http::header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
            .header(http::header::AUTHORIZATION, "Bearer other")
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let body: Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "auth.malformed_header");
    }

    #[tokio::test]
    async fn bootstrap_api_malformed_json_uses_error_envelope() {
        let app = app_with_session(test_session("init_malformed"));
        let (status, body) = request_raw_json(
            app,
            Method::POST,
            "/v1/init/sessions",
            "{not-json",
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["ok"], false);
        assert!(body["error"]["code"].is_string());
    }

    #[tokio::test]
    async fn bootstrap_native_config_cancel_guards_session_state() {
        const CANCEL_BODY: &str = r#"{"operation_id":"nci_init_deadbeefdeadbeefdeadbeef","revision":"0000000000000000000000000000000000000000000000000000000000000000"}"#;

        let app = app_with_session(test_session("init_nc_cancel"));
        let (status, body) = request_raw_json(
            app,
            Method::POST,
            "/v1/init/sessions/unknown/native-config/cancel",
            CANCEL_BODY,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "init.session_not_found");

        // A running session has not published a result yet, so there is no
        // applied import to roll back.
        let app = app_with_session(test_session("init_nc_cancel"));
        let (status, body) = request_raw_json(
            app,
            Method::POST,
            "/v1/init/sessions/init_nc_cancel/native-config/cancel",
            CANCEL_BODY,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "init.result_unavailable");
    }

    #[test]
    fn parse_optional_duration_accepts_suffixes_and_zero_disables() {
        assert_eq!(
            parse_optional_duration("15m", "idle timeout").expect("15m parses"),
            Some(std::time::Duration::from_secs(900))
        );
        assert_eq!(
            parse_optional_duration("0s", "idle timeout").expect("0s parses"),
            None
        );
        assert!(parse_optional_duration("banana", "idle timeout").is_err());
    }

    fn reaper_test_manager(session_id: &str) -> (Arc<HostedInitManager>, Arc<HostedInitSession>) {
        let manager = HostedInitManager::new();
        let session = HostedInitSession::new(session_id.to_owned(), manager.shutdown.clone());
        *lock_unpoisoned(&manager.active) = Some(session.clone());
        (manager, session)
    }

    #[tokio::test(start_paused = true)]
    async fn idle_reaper_expires_abandoned_session() {
        let (manager, session) = reaper_test_manager("init_idle_reap");
        tokio::spawn(reap_idle_session(
            manager.clone(),
            Some(std::time::Duration::from_secs(10)),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            manager.wait_for_terminal(),
        )
        .await
        .expect("idle reaper should shut the server down");
        assert_eq!(session.status(), "canceled");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_reaper_skips_sessions_with_connected_websocket() {
        let (manager, session) = reaper_test_manager("init_idle_ws");
        session.ws_connected();
        tokio::spawn(reap_idle_session(
            manager.clone(),
            Some(std::time::Duration::from_secs(10)),
        ));
        // A listen-only backend holds the socket past the timeout; the
        // session must survive.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        assert_eq!(session.status(), "running");
        session.ws_disconnected();
        // Disconnect restarts the idle clock so a dropped backend gets the
        // full timeout to reconnect and ack before the reaper fires.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        assert_eq!(session.status(), "running");
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            manager.wait_for_terminal(),
        )
        .await
        .expect("reaper should fire once the reconnect grace lapses");
        assert_eq!(session.status(), "canceled");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_reaper_respects_route_lookup_activity() {
        let (manager, session) = reaper_test_manager("init_idle_poll");
        let app = app_with_manager(manager.clone());
        tokio::spawn(reap_idle_session(
            manager.clone(),
            Some(std::time::Duration::from_secs(10)),
        ));
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        // Polling the status endpoint is API activity; it is what keeps a
        // REST-polling backend's session alive.
        let (status, _) = request_json(
            app,
            Method::GET,
            "/v1/init/sessions/init_idle_poll",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        assert_eq!(session.status(), "running");
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            manager.wait_for_terminal(),
        )
        .await
        .expect("reaper should fire after polling stops");
        assert_eq!(session.status(), "canceled");
    }

    #[tokio::test(start_paused = true)]
    async fn status_reports_idle_age_before_counting_itself() {
        let (manager, _session) = reaper_test_manager("init_age");
        let app = app_with_manager(manager);
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let (status, body) = request_json(
            app.clone(),
            Method::GET,
            "/v1/init/sessions/init_age",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The age is the idleness leading up to the poll; the poll itself
        // must not reset the value it reports.
        assert_eq!(body["data"]["last_activity_age_secs"], 30);
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let (_, body) = request_json(
            app,
            Method::GET,
            "/v1/init/sessions/init_age",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(body["data"]["last_activity_age_secs"], 5);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_reaper_respects_pre_session_api_activity() {
        let manager = HostedInitManager::new();
        let app = app_with_manager(manager.clone());
        tokio::spawn(reap_idle_session(
            manager.clone(),
            Some(std::time::Duration::from_secs(10)),
        ));
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        // Even a 404 poll for a not-yet-created session is authenticated API
        // activity and restarts the pre-session idle clock.
        let (status, _) = request_json(
            app,
            Method::GET,
            "/v1/init/sessions/init_unknown",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                manager.wait_for_terminal()
            )
            .await
            .is_err(),
            "active backend polling must hold off the pre-session idle shutdown"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            manager.wait_for_terminal(),
        )
        .await
        .expect("server should idle out once polling stops");
        let error = manager
            .terminal_result()
            .expect_err("pre-session idle-out must exit non-zero");
        assert!(error.public_message().contains("idle_timeout"));
    }

    #[tokio::test]
    async fn shutdown_if_no_session_is_atomic_with_session_creation() {
        let manager = HostedInitManager::new();
        let session = HostedInitSession::new("init_atomic".to_owned(), manager.shutdown.clone());
        *lock_unpoisoned(&manager.active) = Some(session);
        assert!(!manager.shutdown_if_no_session("idle_timeout"));
        assert!(manager.terminal_result().is_ok());

        let empty = HostedInitManager::new();
        assert!(empty.shutdown_if_no_session("idle_timeout"));
        tokio::time::timeout(std::time::Duration::from_secs(1), empty.wait_for_terminal())
            .await
            .expect("shutdown should have fired");
        assert!(empty.terminal_result().is_err());
    }

    #[tokio::test]
    async fn websocket_closes_when_session_turns_terminal() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let (manager, session) = reaper_test_manager("init_ws_terminal");
        let app = app_with_manager(manager);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let url = format!("ws://{addr}/v1/init/sessions/init_ws_terminal/ws");
        let mut request = url.as_str().into_client_request().expect("ws request");
        request.headers_mut().insert(
            http::header::AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().expect("auth header"),
        );
        let (mut stream, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("ws connect");
        let hello = stream.next().await.expect("hello frame").expect("hello ok");
        assert!(hello.is_text());
        let hello: Value =
            serde_json::from_str(hello.to_text().expect("hello text")).expect("hello json");
        assert_eq!(hello["state"]["categories"][0]["id"], json!("agent"));

        // State transitions reach a real socket, not just the history.
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        });
        let state = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("timed out waiting for the state frame")
            .expect("stream ended before the state frame")
            .expect("frame");
        let state: Value =
            serde_json::from_str(state.to_text().expect("state text")).expect("state json");
        assert_eq!(state["type"], json!("state"));
        assert_eq!(state["categories"][0]["value"], json!("opencode"));

        // A reaper expiry while a client holds the socket must end the
        // connection server-side; waiting on the client would let a hung
        // backend pin the process past --max-lifetime.
        session.expire("max_lifetime");

        let mut saw_canceled = false;
        loop {
            let message = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                .await
                .expect("timed out waiting for server-side close")
                .expect("stream ended before a close frame")
                .expect("frame");
            if let tokio_tungstenite::tungstenite::Message::Text(text) = &message {
                assert!(text.contains("canceled"));
                saw_canceled = true;
            } else if message.is_close() {
                break;
            }
        }
        assert!(saw_canceled);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_reaper_respects_activity_touch() {
        let (manager, session) = reaper_test_manager("init_idle_touch");
        tokio::spawn(reap_idle_session(
            manager.clone(),
            Some(std::time::Duration::from_secs(10)),
        ));
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        session.touch();
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        assert_eq!(session.status(), "running");
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            manager.wait_for_terminal(),
        )
        .await
        .expect("reaper should fire after activity stops");
        assert_eq!(session.status(), "canceled");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_reaper_expires_server_without_session() {
        let manager = HostedInitManager::new();
        tokio::spawn(reap_idle_session(
            manager.clone(),
            Some(std::time::Duration::from_secs(10)),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            manager.wait_for_terminal(),
        )
        .await
        .expect("server with no session should idle out");
        let error = manager
            .terminal_result()
            .expect_err("no-session idle-out must exit non-zero");
        assert!(error.public_message().contains("idle_timeout"));
    }

    #[tokio::test(start_paused = true)]
    async fn max_lifetime_enforcer_expires_active_session() {
        let (manager, session) = reaper_test_manager("init_max_lifetime");
        tokio::spawn(enforce_max_lifetime(
            manager.clone(),
            std::time::Duration::from_secs(5),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            manager.wait_for_terminal(),
        )
        .await
        .expect("max lifetime should shut the server down");
        assert_eq!(session.status(), "canceled");
    }

    #[tokio::test(start_paused = true)]
    async fn max_lifetime_enforcer_shuts_down_server_without_session() {
        let manager = HostedInitManager::new();
        tokio::spawn(enforce_max_lifetime(
            manager.clone(),
            std::time::Duration::from_secs(5),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            manager.wait_for_terminal(),
        )
        .await
        .expect("max lifetime should shut down a server with no session");
        let error = manager
            .terminal_result()
            .expect_err("no-session max-lifetime shutdown must exit non-zero");
        assert!(error.public_message().contains("max_lifetime"));
    }

    #[test]
    fn expire_clears_unacked_result_and_secrets() {
        let session = test_session("init_expire");
        session.set_result(json!({
            "status": "initialized",
            "session_key": "acps_session_expire_secret",
            "admin_key": "acps_admin_expire_secret"
        }));
        assert_eq!(session.status(), "completed_awaiting_ack");
        // Backend-driven cancel must not kill a session holding an un-acked
        // result; only the internal reaper may.
        session.cancel("backend_cancel");
        assert_eq!(session.status(), "completed_awaiting_ack");
        session.expire("idle_timeout");
        assert_eq!(session.status(), "canceled");
        assert!(session.result_frame().is_none());
        let snapshot = serde_json::to_string(&session.status_snapshot()).expect("snapshot");
        assert!(snapshot.contains("last_activity_age_secs"));
        assert!(!snapshot.contains("acps_session_expire_secret"));
        let events = serde_json::to_string(&session.events_after(0)).expect("events");
        assert!(events.contains("idle_timeout"));
        assert!(!events.contains("acps_session_expire_secret"));
        // A second expiry is a no-op on an already terminal session.
        session.expire("max_lifetime");
        assert_eq!(session.status(), "canceled");
    }

    // Session state model: the `state` frame, the snapshot embedded in hello
    // and the REST status, and the signal-to-status derivation behind both.

    /// Every recorded `state` event, in seq order.
    fn state_events(session: &HostedInitSession) -> Vec<Value> {
        session
            .events_after(0)
            .into_iter()
            .filter(|event| event["type"] == json!("state"))
            .collect()
    }

    fn latest_state(session: &HostedInitSession) -> Value {
        state_events(session)
            .pop()
            .expect("session recorded no state event")
    }

    fn category<'a>(state: &'a Value, id: &str) -> &'a Value {
        state["categories"]
            .as_array()
            .expect("state must carry a category array")
            .iter()
            .find(|entry| entry["id"] == json!(id))
            .unwrap_or_else(|| panic!("category `{id}` is missing from the snapshot"))
    }

    fn category_ids(state: &Value) -> Vec<String> {
        state["categories"]
            .as_array()
            .expect("state must carry a category array")
            .iter()
            .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn awaiting_ids(state: &Value) -> Vec<String> {
        state["categories"]
            .as_array()
            .expect("state must carry a category array")
            .iter()
            .filter(|entry| entry["status"] == json!("awaiting_input"))
            .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    const CANONICAL_CATEGORY_IDS: [&str; 9] = [
        "agent",
        "provider",
        "model",
        "mode",
        "workspace",
        "native_config",
        "mcp",
        "skills",
        "deps",
    ];

    #[tokio::test]
    async fn state_snapshot_rides_hello_and_rest_status() {
        let session = test_session("init_state_rest");
        let fresh: Value =
            serde_json::from_str(&session.hello_frame()).expect("hello must be json");
        assert_eq!(category_ids(&fresh["state"]), CANONICAL_CATEGORY_IDS);
        assert_eq!(fresh["state"]["current_step"], Value::Null);

        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::AGENT_INSTALL,
        });
        let hello: Value =
            serde_json::from_str(&session.hello_frame()).expect("hello must be json");
        assert_eq!(hello["state"]["current_step"], json!("agent_install"));

        let app = app_with_session(session.clone());
        let (status, body) = request_json(
            app,
            Method::GET,
            "/v1/init/sessions/init_state_rest",
            None,
            Some(TEST_TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // WebSocket and REST clients must not have to reconcile two shapes.
        assert_eq!(body["data"]["state"], hello["state"]);
    }

    #[test]
    fn each_transition_emits_exactly_one_state_frame() {
        let session = test_session("init_state_transitions");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::WORKSPACE_MATERIALIZE,
        });
        driver.progress("materializing workspace".to_owned());
        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::WORKSPACE_MATERIALIZE,
            disposition: StepDisposition::Executed,
            error_code: None,
        });
        driver.progress("workspace ready".to_owned());
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        });

        let seqs = session
            .events_after(0)
            .iter()
            .map(|event| event["seq"].as_u64().unwrap_or_default())
            .collect::<Vec<_>>();
        assert!(
            seqs.windows(2).all(|pair| pair[1] > pair[0]),
            "seq must stay strictly monotonic across interleaved frames: {seqs:?}"
        );
        assert_eq!(state_events(&session).len(), 3);
    }

    #[test]
    fn input_required_is_followed_immediately_by_its_state_frame() {
        let session = test_session("init_state_prompt");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = hosted_test_request(
            HostedPromptKind::ProviderId,
            HostedPromptStyle::Select,
            "provider",
            &["openrouter"],
        );
        let handle = std::thread::spawn(move || driver.select(request));
        let pending = wait_for_pending_input(&session);

        let events = session.events_after(0);
        let prompt_index = events
            .iter()
            .position(|event| event["type"] == json!("input_required"))
            .expect("input_required must be recorded");
        let announced = &events[prompt_index + 1];
        assert_eq!(announced["type"], json!("state"));
        assert_eq!(
            announced["seq"].as_u64(),
            events[prompt_index]["seq"].as_u64().map(|seq| seq + 1),
            "the state frame must sit directly behind the prompt it describes"
        );
        assert_eq!(awaiting_ids(announced), ["provider"]);

        session
            .submit_input(&pending.request_id, json!(0))
            .expect("submit input");
        handle.join().expect("driver thread").expect("selection");
        // Two transitions, two frames: the prompt going up, and the wizard
        // thread waking to release it. Accepting the answer is not itself a
        // transition — the frontier only moves once the wizard resumes.
        assert_eq!(state_events(&session).len(), 2);
        assert!(awaiting_ids(&latest_state(&session)).is_empty());
    }

    #[test]
    fn at_most_one_category_awaits_input_across_the_whole_surface() {
        let session = test_session("init_state_single_await");
        for (kind, expected) in [
            (HostedPromptKind::ProviderId, "provider"),
            (HostedPromptKind::Model, "model"),
        ] {
            let request =
                hosted_test_request(kind, HostedPromptStyle::Select, "pick one", &["only"]);
            let driver = SessionPromptDriver {
                session: session.clone(),
            };
            let handle = std::thread::spawn(move || driver.select(request));
            let pending = wait_for_pending_input(&session);
            let hello: Value =
                serde_json::from_str(&session.hello_frame()).expect("hello must be json");
            assert_eq!(awaiting_ids(&hello["state"]), [expected]);
            assert_eq!(
                hello["pending_input"]["kind"],
                json!(kind.as_str()),
                "the awaiting category must be the pending prompt's own"
            );
            session
                .submit_input(&pending.request_id, json!(0))
                .expect("submit input");
            handle.join().expect("driver thread").expect("selection");
        }
        for state in state_events(&session) {
            assert!(
                awaiting_ids(&state).len() <= 1,
                "a frame claimed two awaiting categories: {state}"
            );
        }
    }

    #[test]
    fn secret_answers_never_reach_the_state_surface() {
        const SECRET: &str = "sk-hosted-state-secret";
        let session = test_session("init_state_secret");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            kind: HostedPromptKind::ProviderApiKeyValue,
            style: HostedPromptStyle::Password,
            prompt: "OPENROUTER_API_KEY".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
            inspection: None,
        };
        let handle = std::thread::spawn(move || driver.password(request));
        let pending = wait_for_pending_input(&session);
        session
            .submit_input(&pending.request_id, json!(SECRET))
            .expect("submit password");
        handle.join().expect("driver thread").expect("password");
        // Settlement names the provider that was written, never the answer:
        // the signal is emitted at the config-write site, which carries the
        // provider id it just wrote.
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Provider,
            value: Some("openrouter".to_owned()),
        });

        let state = latest_state(&session);
        assert_eq!(category(&state, "provider")["value"], json!("openrouter"));
        let history = serde_json::to_string(&session.events_after(0)).expect("history");
        let hello = session.hello_frame();
        let status = serde_json::to_string(&session.status_snapshot()).expect("status");
        for surface in [&history, &hello, &status] {
            assert!(!surface.contains(SECRET), "secret leaked into {surface}");
        }
        // The prompt named the ref, so history keeps it; the settled snapshot
        // that hello and status carry does not repeat it.
        assert!(history.contains("OPENROUTER_API_KEY"));
        assert!(hello.contains("openrouter"));
        assert!(status.contains("openrouter"));
    }

    #[test]
    fn replay_after_seq_returns_state_frames_in_order() {
        let session = test_session("init_state_replay");
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        });
        let after = session
            .events_after(0)
            .last()
            .and_then(|event| event["seq"].as_u64())
            .expect("a recorded seq");
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Provider,
            value: Some("openrouter".to_owned()),
        });
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Model,
            value: Some("deepseek-v4-flash".to_owned()),
        });

        let replayed = session.events_after(after);
        let seqs = replayed
            .iter()
            .map(|event| event["seq"].as_u64().unwrap_or_default())
            .collect::<Vec<_>>();
        assert!(seqs.windows(2).all(|pair| pair[1] > pair[0]), "{seqs:?}");
        let settled = replayed
            .iter()
            .filter(|event| event["type"] == json!("state"))
            .map(|event| category(event, "model")["value"].clone())
            .collect::<Vec<_>>();
        assert_eq!(settled, [Value::Null, json!("deepseek-v4-flash")]);
    }

    #[test]
    fn probe_verdict_flips_mcp_and_outranks_the_registry() {
        let session = test_session("init_state_probe");
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("placebo".to_owned()),
        });
        let ready = latest_state(&session);
        assert_eq!(category(&ready, "mcp")["status"], json!("ready"));
        // A lane that is still live explains nothing: `reason` says why a
        // category is hidden, so it rides only with `not_applicable`.
        assert_eq!(category(&ready, "mcp")["reason"], Value::Null);

        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mcp,
            applicable: false,
            source: ApplicabilitySource::Probe,
            reason: Some("agent does not advertise MCP support".to_owned()),
        });
        let corrected = latest_state(&session);
        assert_eq!(
            category(&corrected, "mcp")["status"],
            json!("not_applicable")
        );
        assert_eq!(
            category(&corrected, "mcp")["reason"],
            json!("agent does not advertise MCP support"),
            "a hidden lane must say what hid it"
        );

        // The installed harness is the authority: a registry claim arriving
        // afterwards must not resurrect the lane.
        let before = corrected["seq"].as_u64();
        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mcp,
            applicable: true,
            source: ApplicabilitySource::Registry,
            reason: None,
        });
        let latest = latest_state(&session);
        assert_eq!(latest["seq"].as_u64(), before);
        assert_eq!(category(&latest, "mcp")["status"], json!("not_applicable"));
        // The outranked verdict is refused as one write group: had the reason
        // been cleared while the verdict stood, the lane would still hide but
        // could no longer say what hid it.
        assert_eq!(
            category(&latest, "mcp")["reason"],
            json!("agent does not advertise MCP support")
        );
    }

    // Applicability is a claim about whether init will drive a lane, and it can
    // arrive after the lane already ran: the Kimi model pin writes its model
    // before `session/new` reports which values the harness advertises, and
    // that discovery pass retracts a lane it finds nothing for. A lane that
    // wrote a value demonstrably applied, so the retraction is refused rather
    // than erasing what landed in config.
    #[test]
    fn a_late_inapplicable_verdict_cannot_retract_a_settled_lane() {
        let session = test_session("init_state_late_retraction");
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Model,
            value: Some("kimi-k2-thinking".to_owned()),
        });
        let before = latest_state(&session)["seq"].as_u64();

        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Model,
            applicable: false,
            source: ApplicabilitySource::Discovery,
            reason: Some("agent advertised no models".to_owned()),
        });

        let latest = latest_state(&session);
        assert_eq!(latest["seq"].as_u64(), before, "nothing observable moved");
        assert_eq!(category(&latest, "model")["status"], json!("settled"));
        assert_eq!(
            category(&latest, "model")["value"],
            json!("kimi-k2-thinking")
        );
        // The refused verdict must leave no trace: a `reason` on a settled lane
        // would tell the client a lane it can see was ruled out.
        assert_eq!(category(&latest, "model")["reason"], Value::Null);
    }

    // The mirror case: a retraction that arrives before the lane breaks is real
    // when it lands, but the failure that follows is the last word.
    #[test]
    fn a_failure_after_an_inapplicable_verdict_still_displays_failed() {
        let session = test_session("init_state_failure_after_retraction");
        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mode,
            applicable: false,
            source: ApplicabilitySource::Discovery,
            reason: Some("agent advertised no modes".to_owned()),
        });
        assert_eq!(
            category(&latest_state(&session), "mode")["status"],
            json!("not_applicable")
        );

        session.apply_state_signal(InitStateSignal::CategoryFailed {
            category: InitCategory::Mode,
            code: "init.mode_write_failed".to_owned(),
        });

        let latest = latest_state(&session);
        assert_eq!(category(&latest, "mode")["status"], json!("failed"));
        assert_eq!(
            category(&latest, "mode")["code"],
            json!("init.mode_write_failed")
        );
    }

    // A step that fails without parking the session badges its lane from the
    // `StepFinished` signal alone, and only once: the failure and the step
    // ending are one transition.
    #[test]
    fn a_failed_step_badges_its_category_once_on_a_live_session() {
        let session = test_session("init_state_step_failure");
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::PROVIDER_CONFIGURE,
        });
        let before = state_events(&session).len();

        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::PROVIDER_CONFIGURE,
            disposition: StepDisposition::Executed,
            error_code: Some("init.provider_write_failed".to_owned()),
        });

        assert_eq!(state_events(&session).len(), before + 1);
        let latest = latest_state(&session);
        assert_eq!(category(&latest, "provider")["status"], json!("failed"));
        assert_eq!(
            category(&latest, "provider")["code"],
            json!("init.provider_write_failed")
        );
        assert!(session.is_active());
    }

    // `provider_configure` owns three lanes, and the model and mode lanes badge
    // themselves before the error leaves the step. The step then reports the
    // same error on its way out, and must not read as a second, provider-shaped
    // failure on top of the blame that was already assigned.
    #[test]
    fn a_step_failure_leaves_a_lane_that_already_took_the_blame_alone() {
        let session = test_session("init_state_step_failure_attributed");
        // The provider lane settles at its own write site, inside the step and
        // ahead of the model lane that goes on to break.
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Provider,
            value: Some("openrouter".to_owned()),
        });
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::PROVIDER_CONFIGURE,
        });
        session.apply_state_signal(InitStateSignal::CategoryFailed {
            category: InitCategory::Model,
            code: "init.model_write_failed".to_owned(),
        });

        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::PROVIDER_CONFIGURE,
            disposition: StepDisposition::Executed,
            error_code: Some("init.model_write_failed".to_owned()),
        });

        let latest = latest_state(&session);
        assert_eq!(category(&latest, "model")["status"], json!("failed"));
        assert_eq!(
            category(&latest, "provider")["status"],
            json!("settled"),
            "the provider lane finished before the model lane broke"
        );
        assert_eq!(category(&latest, "provider")["value"], json!("openrouter"));
    }

    // `failed` outranks `not_applicable`, so a step badging a lane this run does
    // not have would invent a broken lane. The terminal error frame and
    // `current_step` are what carry such a failure.
    #[test]
    fn a_step_failure_never_badges_a_lane_this_run_does_not_have() {
        let session = test_session("init_state_step_failure_absent_lane");
        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Provider,
            applicable: false,
            source: ApplicabilitySource::Registry,
            reason: Some("agent does not take a provider".to_owned()),
        });
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::PROVIDER_CONFIGURE,
        });

        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::PROVIDER_CONFIGURE,
            disposition: StepDisposition::Executed,
            error_code: Some("init.secret_store_unavailable".to_owned()),
        });

        let latest = latest_state(&session);
        assert_eq!(
            category(&latest, "provider")["status"],
            json!("not_applicable")
        );
        assert_eq!(
            category(&latest, "provider")["reason"],
            json!("agent does not take a provider")
        );

        // The same holds for the mode-only lane shape, where the blame was
        // assigned explicitly and the step is echoing it.
        session.apply_state_signal(InitStateSignal::CategoryFailed {
            category: InitCategory::Mode,
            code: "init.mode_write_failed".to_owned(),
        });
        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::PROVIDER_CONFIGURE,
            disposition: StepDisposition::Executed,
            error_code: Some("init.mode_write_failed".to_owned()),
        });
        let latest = latest_state(&session);
        assert_eq!(
            category(&latest, "provider")["status"],
            json!("not_applicable")
        );
        assert_eq!(category(&latest, "mode")["status"], json!("failed"));
    }

    // The other direction: nothing claimed the failure, so the step's own lane
    // is the only thing that can carry it — settled or not. The MCP lane settles
    // at the probe, well before the write that can still break.
    #[test]
    fn an_unclaimed_step_failure_badges_its_lane_over_a_settlement() {
        let session = test_session("init_state_step_failure_over_settlement");
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Mcp,
            value: Some("linear".to_owned()),
        });
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::MCP_CONFIGURE,
        });

        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::MCP_CONFIGURE,
            disposition: StepDisposition::Executed,
            error_code: Some("init.mcp_write_failed".to_owned()),
        });

        let latest = latest_state(&session);
        assert_eq!(category(&latest, "mcp")["status"], json!("failed"));
        assert_eq!(
            category(&latest, "mcp")["code"],
            json!("init.mcp_write_failed")
        );
    }

    // The mirror of the retraction guard: a settlement read off the config that
    // was already on disk is a report, not evidence the lane exists. When the
    // installed agent has since dropped the lane, the live discovery pass is
    // what knows, and the stale value goes with the withdrawn lane.
    #[test]
    fn discovery_withdraws_a_settlement_carried_over_from_existing_config() {
        let session = test_session("init_state_provisional_retraction");
        session.apply_state_signal(InitStateSignal::CategoryProvisionallySettled {
            category: InitCategory::Mode,
            value: "smart".to_owned(),
        });
        let carried = latest_state(&session);
        assert_eq!(category(&carried, "mode")["status"], json!("settled"));
        assert_eq!(category(&carried, "mode")["value"], json!("smart"));

        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mode,
            applicable: false,
            source: ApplicabilitySource::Discovery,
            reason: Some("agent advertised no `mode` values on session/new".to_owned()),
        });

        let latest = latest_state(&session);
        assert_eq!(category(&latest, "mode")["status"], json!("not_applicable"));
        assert_eq!(
            category(&latest, "mode")["reason"],
            json!("agent advertised no `mode` values on session/new")
        );
        assert_eq!(
            category(&latest, "mode")["value"],
            Value::Null,
            "a withdrawn lane must not keep the value it no longer has"
        );
    }

    // A provisional settlement rests on the config, and a check that never ran
    // is no evidence against it. The mode-only discovery lane swallows a harness
    // that will not open a provisional session, and that skip must not report a
    // mode the config genuinely holds as a lane the agent does not have.
    #[test]
    fn an_unavailable_discovery_check_withdraws_nothing_the_config_holds() {
        let session = test_session("init_state_discovery_unavailable");
        session.apply_state_signal(InitStateSignal::CategoryProvisionallySettled {
            category: InitCategory::Mode,
            value: "smart".to_owned(),
        });
        let before = latest_state(&session)["seq"].as_u64();

        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mode,
            applicable: false,
            source: ApplicabilitySource::DiscoveryUnavailable,
            reason: Some("mode discovery skipped: agent exited".to_owned()),
        });

        let latest = latest_state(&session);
        assert_eq!(latest["seq"].as_u64(), before, "nothing observable moved");
        assert_eq!(category(&latest, "mode")["status"], json!("settled"));
        assert_eq!(category(&latest, "mode")["value"], json!("smart"));
        assert_eq!(category(&latest, "mode")["reason"], Value::Null);
    }

    // With nothing on the lane, the same verdict is the whole story: the run
    // will not discover a mode and none is configured, so the lane must read as
    // absent with the skip reason rather than staying open forever.
    #[test]
    fn an_unavailable_discovery_check_still_closes_a_lane_with_no_outcome() {
        let session = test_session("init_state_discovery_unavailable_open_lane");
        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mode,
            applicable: false,
            source: ApplicabilitySource::DiscoveryUnavailable,
            reason: Some("mode discovery skipped: agent exited".to_owned()),
        });

        let latest = latest_state(&session);
        assert_eq!(category(&latest, "mode")["status"], json!("not_applicable"));
        assert_eq!(
            category(&latest, "mode")["reason"],
            json!("mode discovery skipped: agent exited")
        );

        // And the registry does not get to claim the lane back afterwards: the
        // harness is what failed to produce it.
        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mode,
            applicable: true,
            source: ApplicabilitySource::Registry,
            reason: None,
        });
        assert_eq!(
            category(&latest_state(&session), "mode")["status"],
            json!("not_applicable")
        );
    }

    // A lane that is really driven re-settles at its write site, which is what
    // makes the carried-over report final: from there it is this run's own
    // evidence and no verdict takes it back.
    #[test]
    fn a_write_site_settlement_promotes_a_carried_over_one() {
        let session = test_session("init_state_provisional_promotion");
        session.apply_state_signal(InitStateSignal::CategoryProvisionallySettled {
            category: InitCategory::Model,
            value: "kimi-k2-thinking".to_owned(),
        });
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Model,
            value: Some("kimi-k2-thinking".to_owned()),
        });

        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Model,
            applicable: false,
            source: ApplicabilitySource::Discovery,
            reason: Some("agent advertised no models".to_owned()),
        });

        let latest = latest_state(&session);
        assert_eq!(category(&latest, "model")["status"], json!("settled"));
        assert_eq!(
            category(&latest, "model")["value"],
            json!("kimi-k2-thinking")
        );
        assert_eq!(category(&latest, "model")["reason"], Value::Null);
    }

    // The terminal sweep means "init finished and nothing is left to drive", so
    // a failed final step must leave the lanes it never reached alone rather
    // than reporting them as settled with nothing behind them.
    #[test]
    fn a_failed_init_complete_runs_no_terminal_sweep() {
        let session = test_session("init_state_failed_complete");
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        });

        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::INIT_COMPLETE,
            disposition: StepDisposition::Executed,
            error_code: Some("init.finalize_failed".to_owned()),
        });

        let latest = latest_state(&session);
        for id in ["workspace", "native_config", "deps", "mcp", "skills"] {
            assert_eq!(
                category(&latest, id)["status"],
                json!("ready"),
                "`{id}` was never driven, so a failed completion must not settle it"
            );
        }
    }

    #[test]
    fn failure_badges_its_category_before_the_terminal_error_frame() {
        let session = test_session("init_state_failure");
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::MCP_CONFIGURE,
        });
        session.set_error(
            "init.mcp_write_failed",
            "mcp config write failed".to_owned(),
        );

        let events = session.events_after(0);
        let tail = &events[events.len() - 2..];
        assert_eq!(tail[0]["type"], json!("state"));
        assert_eq!(tail[1]["type"], json!("error"));
        assert_eq!(category(&tail[0], "mcp")["status"], json!("failed"));
        assert_eq!(
            category(&tail[0], "mcp")["code"],
            json!("init.mcp_write_failed")
        );
        // A parked failure is still live: the backend has to be able to
        // replay and acknowledge it.
        assert_eq!(session.status(), "errored");
        assert!(session.is_active());
    }

    #[test]
    fn a_failure_between_steps_leaves_the_settled_category_alone() {
        let session = test_session("init_state_between_steps");
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::MCP_CONFIGURE,
        });
        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::MCP_CONFIGURE,
            disposition: StepDisposition::Executed,
            error_code: None,
        });
        let settled = state_events(&session).len();
        // `current_step` still names `mcp_configure` for the wire, but the step
        // is over: a failure surfacing between steps belongs to no lane.
        session.set_error(
            "init.config_reload_failed",
            "config reload failed".to_owned(),
        );

        let frontier = latest_state(&session);
        assert_eq!(
            state_events(&session).len(),
            settled,
            "a failure owning no live step must not move the frontier"
        );
        assert_eq!(category(&frontier, "mcp")["status"], json!("settled"));
        for id in CANONICAL_CATEGORY_IDS {
            assert_ne!(
                category(&frontier, id)["status"],
                json!("failed"),
                "category `{id}` was badged by a failure that owned no step"
            );
        }
        assert_eq!(session.status(), "errored");
    }

    #[test]
    fn a_cancel_mid_prompt_freezes_the_category_frontier() {
        // The wizard thread does not stop where the cancel lands: it unwinds
        // through the lane's own failure badge and the step's finish signal.
        // Neither may record a frame after the terminal one, and neither may
        // move the snapshot `hello` and the status route derive live.
        let session = test_session("init_state_cancel_freeze");
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::PROVIDER_CONFIGURE,
        });
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = hosted_test_request(
            HostedPromptKind::ProviderId,
            HostedPromptStyle::Select,
            "select a provider",
            &["openrouter"],
        );
        let handle = std::thread::spawn(move || driver.select(request));
        wait_for_pending_input(&session);
        session.cancel("client canceled");
        handle
            .join()
            .expect("driver thread")
            .expect_err("a canceled session must release the pending prompt");

        session.apply_state_signal(InitStateSignal::CategoryFailed {
            category: InitCategory::Provider,
            code: "init.canceled".to_owned(),
        });
        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::PROVIDER_CONFIGURE,
            disposition: StepDisposition::Executed,
            error_code: Some("init.canceled".to_owned()),
        });

        let events = session.events_after(0);
        assert_eq!(
            events.last().expect("session recorded no events")["type"],
            json!("canceled"),
            "the cancellation must be the last thing the client is told: {events:?}"
        );
        let hello: Value =
            serde_json::from_str(&session.hello_frame()).expect("hello must be json");
        for frontier in [latest_state(&session), hello["state"].clone()] {
            for id in CANONICAL_CATEGORY_IDS {
                assert_ne!(
                    category(&frontier, id)["status"],
                    json!("failed"),
                    "category `{id}` was badged failed after the session was canceled"
                );
            }
        }
    }

    #[test]
    fn a_cross_cutting_prompt_records_input_required_with_no_state_frame() {
        // `secret_ref_value` belongs to no category, so nothing derives as
        // `awaiting_input` for it and the snapshot does not move: the prompt is
        // announced by `input_required` alone.
        let session = test_session("init_state_cross_cutting");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
            kind: HostedPromptKind::SecretRefValue,
            style: HostedPromptStyle::Password,
            prompt: "LINEAR_API_KEY".to_owned(),
            required: false,
            default: None,
            items: Vec::new(),
            inspection: None,
        };
        let handle = std::thread::spawn(move || driver.password(request));
        let pending = wait_for_pending_input(&session);
        assert!(
            state_events(&session).is_empty(),
            "a category-less prompt must raise no state frame"
        );
        assert_eq!(
            session
                .events_after(0)
                .last()
                .expect("session recorded no events")["type"],
            json!("input_required")
        );
        session
            .submit_input(&pending.request_id, json!(null))
            .expect("submit input");
        handle.join().expect("driver thread").expect("password");
        assert!(
            state_events(&session).is_empty(),
            "answering a category-less prompt must raise no state frame either"
        );
    }

    #[test]
    fn blocked_on_follows_the_dependency_table() {
        let session = test_session("init_state_blocked");
        let fresh: Value = serde_json::from_str(&session.hello_frame()).expect("hello");
        assert_eq!(
            category(&fresh["state"], "model")["blocked_on"],
            json!("provider")
        );
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        });
        let after_agent = latest_state(&session);
        assert_eq!(category(&after_agent, "provider")["status"], json!("ready"));
        assert_eq!(
            category(&after_agent, "model")["blocked_on"],
            json!("provider")
        );

        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Provider,
            value: Some("openrouter".to_owned()),
        });
        let after_provider = latest_state(&session);
        assert_eq!(category(&after_provider, "model")["status"], json!("ready"));
        assert_eq!(
            category(&after_provider, "model")["blocked_on"],
            Value::Null
        );
        assert_eq!(
            category(&after_provider, "mode")["blocked_on"],
            json!("model")
        );

        // An inapplicable dependency unblocks just like a settled one.
        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Model,
            applicable: false,
            source: ApplicabilitySource::Registry,
            reason: Some("agent does not take a model".to_owned()),
        });
        assert_eq!(
            category(&latest_state(&session), "mode")["status"],
            json!("ready")
        );
    }

    #[test]
    fn a_signal_that_changes_nothing_emits_no_frame() {
        let session = test_session("init_state_dedup");
        let settled = || InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        };
        session.apply_state_signal(settled());
        let first = latest_state(&session);
        session.apply_state_signal(settled());
        assert_eq!(state_events(&session).len(), 1);
        assert_eq!(
            session
                .events_after(0)
                .last()
                .and_then(|event| event["seq"].as_u64()),
            first["seq"].as_u64(),
            "a no-op signal must not burn a seq"
        );
    }

    #[test]
    fn history_cap_evicts_state_frames_while_hello_stays_current() {
        let session = test_session("init_state_cap");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        });
        for index in 0..INIT_EVENT_HISTORY_LIMIT + 1 {
            driver.progress(format!("step {index}"));
        }
        assert!(
            state_events(&session).is_empty(),
            "the early state frame should have aged out of the capped history"
        );
        // Which is exactly why hello carries the whole snapshot.
        let hello: Value = serde_json::from_str(&session.hello_frame()).expect("hello");
        assert_eq!(
            category(&hello["state"], "agent")["value"],
            json!("opencode")
        );
    }

    #[test]
    fn init_complete_settles_every_applicable_category_left_open() {
        let session = test_session("init_state_sweep");
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        });
        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mode,
            applicable: false,
            source: ApplicabilitySource::Registry,
            reason: Some("agent does not take a mode".to_owned()),
        });
        let before = state_events(&session).len();
        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::INIT_COMPLETE,
            disposition: StepDisposition::Executed,
            error_code: None,
        });
        // The sweep and the step that triggered it are one transition.
        assert_eq!(state_events(&session).len(), before + 1);

        let swept = latest_state(&session);
        assert_eq!(category(&swept, "agent")["value"], json!("opencode"));
        assert_eq!(category(&swept, "mode")["status"], json!("not_applicable"));
        for id in CANONICAL_CATEGORY_IDS {
            let status = category(&swept, id)["status"].clone();
            assert!(
                status == json!("settled") || status == json!("not_applicable"),
                "category `{id}` still derives as {status} after init completed"
            );
        }
        assert_eq!(category(&swept, "deps")["value"], Value::Null);
    }

    /// The real derivation a hosted run performs the instant its agent is
    /// written, driven through the session so the wire snapshot — not just the
    /// signal list — is what gets asserted.
    fn apply_agent_settlement(session: &HostedInitSession, agent_id: &str, args: &InitArgs) {
        let mut config = settlement_fixture_config();
        config.agent.id = agent_id.to_owned();
        apply_settlement_signals(session, &config, args);
    }

    fn settlement_fixture_config() -> config::Config {
        config::load_config_from_str(include_str!(
            "../../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture config")
    }

    fn apply_settlement_signals(
        session: &HostedInitSession,
        config: &config::Config,
        args: &InitArgs,
    ) {
        let registry = crate::runtime::install::agent_registry::RegistryCatalog::load_embedded()
            .expect("registry");
        for signal in super::super::run::agent_settlement_signals(config, &registry, args, false) {
            session.apply_state_signal(signal);
        }
    }

    #[test]
    fn hosted_custom_agent_settles_the_agent_and_strands_no_harness_lane() {
        let args = request_from_json(
            r#"{
                "custom_agent_id": "housebot",
                "custom_agent_command": "housebot-acp",
                "custom_agent_install": "npm install -g housebot"
            }"#,
        )
        .into_init_args()
        .expect("valid request");
        let session = test_session("init_state_custom_agent");
        apply_agent_settlement(&session, "housebot", &args);

        let state = latest_state(&session);
        assert_eq!(category(&state, "agent")["status"], json!("settled"));
        assert_eq!(category(&state, "agent")["value"], json!("housebot"));
        // A registry-less agent takes its provider, model, mode, and skills
        // from its own environment, so a client must never render those lanes
        // as input that is still coming.
        for id in ["provider", "model", "mode", "skills"] {
            assert_eq!(
                category(&state, id)["status"],
                json!("not_applicable"),
                "custom agents configure `{id}` outside acp-stack"
            );
        }
        // MCP has no registry column; only the live probe may rule on it.
        assert_eq!(category(&state, "mcp")["status"], json!("ready"));
    }

    // A resumed run replays its configuration steps as skipped and a declared
    // run never prompts, so no write site fires on either path. Without the
    // config-derived settlements the harness lanes would report `settled` with
    // a null value, telling a client the run configured nothing.
    #[test]
    fn hosted_settlement_reports_the_harness_values_already_in_the_config() {
        let args = request_from_json(r#"{"resume": true, "agent": "opencode"}"#)
            .into_init_args()
            .expect("valid request");
        let mut config = settlement_fixture_config();
        config.agent.provider = Some(crate::config::AgentProviderConfig {
            id: "openrouter".to_owned(),
            model: Some("deepseek-v4-flash".to_owned()),
            api_key_ref: Some("OPENROUTER_API_KEY".to_owned()),
            custom: None,
        });
        config.agent.mode = Some("smart".to_owned());
        config.mcp.servers = vec![crate::config::McpServerConfig::Stdio(
            crate::config::McpStdioServer {
                name: "linear".to_owned(),
                command: "linear-mcp".to_owned(),
                args: Vec::new(),
                env: Vec::new(),
            },
        )];
        let session = test_session("init_state_declared_values");
        apply_settlement_signals(&session, &config, &args);

        let state = latest_state(&session);
        for (id, value) in [
            ("agent", "opencode"),
            ("provider", "openrouter"),
            // Provider-backed agents keep the model inside `[agent.provider]`.
            ("model", "deepseek-v4-flash"),
            ("mode", "smart"),
        ] {
            assert_eq!(category(&state, id)["status"], json!("settled"), "`{id}`");
            assert_eq!(
                category(&state, id)["value"],
                json!(value),
                "`{id}` must report what is on disk, not null"
            );
        }
        // MCP is the exception: declaring servers says nothing about whether
        // the installed agent can be handed any, so the lane is still open here
        // and the probe is what closes it.
        assert_eq!(category(&state, "mcp")["status"], json!("ready"));

        // The probe's turn. Both of its signals are what the capability step
        // emits, in that order.
        session.apply_state_signal(super::super::run::mcp_applicability_from_probe(
            &super::super::CapabilityProbeOutcome::Probed(mcp_capabilities(json!({"stdio": true}))),
        ));
        let settlement = super::super::run::mcp_settlement_from_probe(
            &mcp_capabilities(json!({"stdio": true})),
            &config,
            &[],
        )
        .expect("a declared server the agent can take settles the lane");
        session.apply_state_signal(settlement);

        let state = latest_state(&session);
        assert_eq!(category(&state, "mcp")["status"], json!("settled"));
        assert_eq!(category(&state, "mcp")["value"], json!("linear"));
    }

    // The case the probe-first ordering exists for: the servers are declared,
    // the installed agent advertises no MCP at all, and runtime will hand it
    // nothing — so the lane must read as absent, not as configured.
    #[test]
    fn a_declared_mcp_server_stays_inapplicable_when_the_agent_advertises_none() {
        let args = request_from_json(r#"{"agent": "opencode"}"#)
            .into_init_args()
            .expect("valid request");
        let mut config = settlement_fixture_config();
        config.mcp.servers = vec![crate::config::McpServerConfig::Stdio(
            crate::config::McpStdioServer {
                name: "linear".to_owned(),
                command: "linear-mcp".to_owned(),
                args: Vec::new(),
                env: Vec::new(),
            },
        )];
        let session = test_session("init_state_declared_mcp_unsupported");
        apply_settlement_signals(&session, &config, &args);

        let silent = mcp_capabilities(json!({}));
        assert_eq!(
            super::super::run::mcp_settlement_from_probe(&silent, &config, &[]),
            None,
            "an agent that takes no MCP servers settles nothing"
        );
        session.apply_state_signal(super::super::run::mcp_applicability_from_probe(
            &super::super::CapabilityProbeOutcome::Probed(silent),
        ));

        let state = latest_state(&session);
        assert_eq!(category(&state, "mcp")["status"], json!("not_applicable"));
        assert_eq!(
            category(&state, "mcp")["reason"],
            json!("agent does not advertise MCP support")
        );
    }

    // The other model slot: an agent with no provider block keeps its model at
    // the config root, and settlement has to read it from there.
    #[test]
    fn hosted_settlement_reads_a_root_model_for_a_provider_less_agent() {
        let args = request_from_json(r#"{"agent": "amp"}"#)
            .into_init_args()
            .expect("valid request");
        let mut config = settlement_fixture_config();
        config.agent.id = "amp".to_owned();
        config.agent.provider = None;
        config.agent.model = Some("gpt-5-codex".to_owned());
        let session = test_session("init_state_root_model");
        apply_settlement_signals(&session, &config, &args);

        let state = latest_state(&session);
        // amp declares `set_model = false`, so the lane still reads as absent —
        // what is asserted is that the settlement carried the root value, which
        // the wire shows the moment a model-taking agent is in the same shape.
        assert_eq!(
            category(&state, "model")["status"],
            json!("not_applicable"),
            "the registry verdict stands over a value init will not rewrite"
        );

        let session = test_session("init_state_root_model_applicable");
        config.agent.id = "opencode".to_owned();
        apply_settlement_signals(&session, &config, &args);
        let state = latest_state(&session);
        assert_eq!(category(&state, "model")["status"], json!("settled"));
        assert_eq!(category(&state, "model")["value"], json!("gpt-5-codex"));
    }

    #[test]
    fn hosted_resume_settles_categories_from_replayed_steps() {
        let args = request_from_json(r#"{"resume": true, "agent": "opencode"}"#)
            .into_init_args()
            .expect("valid request");
        let session = test_session("init_state_resume");
        apply_agent_settlement(&session, "opencode", &args);

        // A resumed run replays already-verified rows as skipped; the category
        // behind each one must settle exactly as an executed step settles it,
        // or the snapshot would report work that will never be driven again.
        for kind in [
            step_kind::AGENT_INSTALL,
            step_kind::WORKSPACE_MATERIALIZE,
            step_kind::PROVIDER_CONFIGURE,
            step_kind::MCP_CONFIGURE,
        ] {
            session.apply_state_signal(InitStateSignal::StepStarted { kind });
            session.apply_state_signal(InitStateSignal::StepFinished {
                kind,
                disposition: StepDisposition::Skipped,
                error_code: None,
            });
        }
        let mid_run = latest_state(&session);
        for id in ["agent", "workspace", "provider", "mcp"] {
            assert_eq!(
                category(&mid_run, id)["status"],
                json!("settled"),
                "a skipped step must settle `{id}`"
            );
        }

        session.apply_state_signal(InitStateSignal::StepFinished {
            kind: step_kind::INIT_COMPLETE,
            disposition: StepDisposition::Skipped,
            error_code: None,
        });
        let completed = latest_state(&session);
        for id in CANONICAL_CATEGORY_IDS {
            let status = completed["categories"]
                .as_array()
                .expect("categories")
                .iter()
                .find(|entry| entry["id"] == json!(id))
                .map(|entry| entry["status"].clone())
                .expect("every category is reported");
            assert!(
                status == json!("settled") || status == json!("not_applicable"),
                "category `{id}` still derives as {status} after a resumed run completed"
            );
        }
        assert!(
            awaiting_ids(&completed).is_empty(),
            "a completed resume must await nothing: {completed}"
        );
    }

    #[test]
    fn parking_a_failure_releases_a_blocked_prompt_and_keeps_the_first_code() {
        // The frame-encode path parks through exactly this call: a payload
        // that will not serialize cannot be constructed from production types,
        // so the semantics it depends on are asserted directly.
        let session = test_session("init_state_park");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = hosted_test_request(
            HostedPromptKind::Model,
            HostedPromptStyle::Text,
            "model",
            &[],
        );
        let handle = std::thread::spawn(move || driver.text(request));
        wait_for_pending_input(&session);
        session.set_error(
            FRAME_ENCODE_FAILED_CODE,
            FRAME_ENCODE_FAILED_MESSAGE.to_owned(),
        );

        let error = handle
            .join()
            .expect("driver thread")
            .expect_err("a parked session must release the blocked prompt");
        assert!(error.to_string().contains(FRAME_ENCODE_FAILED_CODE));
        assert_eq!(session.status(), "errored");
        assert!(
            session
                .error_replay_frame()
                .expect("replay frame")
                .contains(FRAME_ENCODE_FAILED_CODE)
        );
        assert!(awaiting_ids(&latest_state(&session)).is_empty());

        // The error the wizard propagates afterwards is downstream of the
        // parked one and must not replace it.
        session.set_error("init.invalid_param", "prompt failed".to_owned());
        assert!(
            session
                .error_replay_frame()
                .expect("replay frame")
                .contains(FRAME_ENCODE_FAILED_CODE)
        );
    }

    // Golden byte pins for the server→client frame surface. These assert the
    // exact serialized bytes, not just the fields, because the platform proxy
    // and its recorded fixtures read them. Two different key orders are in
    // play and both are load-bearing: seq-bearing events are assembled through
    // a `BTreeMap` and come out alphabetically sorted, while every seq-less
    // frame comes out in declaration order. `agent-client-protocol` turns on
    // `serde_json/preserve_order` for the whole build, so `Map` is insertion
    // ordered and neither order can be assumed to be the other.
    //
    // Frames are read back from the recorded history rather than the broadcast
    // channel: history is written while the session lock is held, so it is
    // race-free against the wizard thread, and the WebSocket sends exactly
    // `frame.to_string()` of the same `Value`.

    /// Bytes of the recorded event at `seq`, as the WebSocket would send them.
    fn recorded_frame(session: &HostedInitSession, seq: u64) -> String {
        session
            .events_after(seq - 1)
            .first()
            .map(Value::to_string)
            .unwrap_or_else(|| panic!("no recorded init event at seq {seq}"))
    }

    #[test]
    fn golden_progress_event_bytes() {
        let session = test_session("init_golden_progress");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        driver.progress("materializing workspace".to_owned());
        // The session constructor records the first progress event itself.
        assert_eq!(
            recorded_frame(&session, 1),
            r#"{"message":"init session started","seq":1,"session_id":"init_golden_progress","type":"progress"}"#
        );
        assert_eq!(
            recorded_frame(&session, 2),
            r#"{"message":"materializing workspace","seq":2,"session_id":"init_golden_progress","type":"progress"}"#
        );
    }

    #[test]
    fn golden_input_required_and_input_accepted_event_bytes() {
        let session = test_session("init_golden_input");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = hosted_test_request(
            HostedPromptKind::Model,
            HostedPromptStyle::SearchableSelect,
            "select a model",
            &["alpha", "beta"],
        );
        let handle = std::thread::spawn(move || driver.select(request));
        let pending = wait_for_pending_input(&session);
        session
            .submit_input(&pending.request_id, json!(1))
            .expect("submit input");
        let outcome = handle
            .join()
            .expect("driver thread")
            .expect("driver result");
        assert!(matches!(outcome, HostedPromptOutcome::Handled(Some(1))));

        // The nested `input` object is the only frame body whose key order
        // comes from a `Serialize` impl rendered into a `Map`, so it is the
        // only one that would silently reorder if `preserve_order` ever went
        // away. Pinning it as bytes (with the per-request `request_id`
        // spliced in) is what makes that visible. `kind` sits after
        // `request_id` and each option's `value` after its `index`.
        assert_eq!(
            recorded_frame(&session, 2),
            format!(
                r#"{{"input":{{"request_id":"{}","kind":"model","style":"searchable_select","prompt":"select a model","required":false,"default":null,"options":[{{"index":0,"value":"id_alpha","label":"alpha","hint":""}},{{"index":1,"value":"id_beta","label":"beta","hint":""}}]}},"seq":2,"session_id":"init_golden_input","type":"input_required"}}"#,
                pending.request_id
            )
        );
        // seq 3 is the state frame the prompt raised (pinned in
        // `golden_state_event_bytes`); the acceptance follows it.
        assert_eq!(
            recorded_frame(&session, 4),
            format!(
                r#"{{"request_id":"{}","seq":4,"session_id":"init_golden_input","type":"input_accepted"}}"#,
                pending.request_id
            )
        );
    }

    #[test]
    fn golden_state_event_bytes() {
        // Two key orders in one frame, both deliberate: the envelope sorts
        // `categories`/`current_step`/`seq`/`session_id`/`type`
        // alphabetically like every other seq-bearing event, while each
        // category object keeps its declared `id`/`status`/`blocked_on`/
        // `value`/`code`/`reason` order.
        let session = test_session("init_golden_state");
        session.apply_state_signal(InitStateSignal::StepStarted {
            kind: step_kind::PROVIDER_CONFIGURE,
        });
        session.apply_state_signal(InitStateSignal::CategorySettled {
            category: InitCategory::Agent,
            value: Some("opencode".to_owned()),
        });
        session.apply_state_signal(InitStateSignal::CategoryApplicability {
            category: InitCategory::Mode,
            applicable: false,
            source: ApplicabilitySource::Registry,
            reason: Some("agent does not take a mode".to_owned()),
        });
        session.apply_state_signal(InitStateSignal::CategoryFailed {
            category: InitCategory::Skills,
            code: "init.skills_install_failed".to_owned(),
        });
        assert_eq!(
            recorded_frame(&session, 5),
            r#"{"categories":[{"id":"agent","status":"settled","value":"opencode"},{"id":"provider","status":"ready"},{"id":"model","status":"blocked","blocked_on":"provider"},{"id":"mode","status":"not_applicable","reason":"agent does not take a mode"},{"id":"workspace","status":"ready"},{"id":"native_config","status":"ready"},{"id":"mcp","status":"ready"},{"id":"skills","status":"failed","code":"init.skills_install_failed"},{"id":"deps","status":"ready"}],"current_step":"provider_configure","seq":5,"session_id":"init_golden_state","type":"state"}"#
        );
    }

    #[test]
    fn golden_result_ready_result_frame_and_result_acked_bytes() {
        let session = test_session("init_golden_result");
        // Nested objects and non-ASCII text pin the `format!` splice: the
        // stored result is forwarded verbatim, never re-encoded or re-ordered.
        session.set_result(json!({
            "note": "héllo ✅",
            "handoff": {"token": "t", "nested": {"a": [1, 2]}}
        }));
        assert_eq!(
            recorded_frame(&session, 2),
            r#"{"seq":2,"session_id":"init_golden_result","status":"completed_awaiting_ack","type":"result_ready"}"#
        );
        assert_eq!(
            session.result_frame().expect("result frame"),
            r#"{"type":"result","session_id":"init_golden_result","payload":{"note":"héllo ✅","handoff":{"token":"t","nested":{"a":[1,2]}}}}"#
        );
        session.ack_result().expect("ack result");
        assert_eq!(
            recorded_frame(&session, 3),
            r#"{"seq":3,"session_id":"init_golden_result","status":"closed","type":"result_acked"}"#
        );
    }

    #[test]
    fn golden_canceled_event_bytes() {
        let session = test_session("init_golden_cancel");
        session.cancel("backend_cancel");
        assert_eq!(
            recorded_frame(&session, 2),
            r#"{"reason":"backend_cancel","seq":2,"session_id":"init_golden_cancel","type":"canceled"}"#
        );
    }

    #[test]
    fn golden_error_replay_and_error_acked_bytes() {
        let session = test_session("init_golden_error");
        session.set_error("init.boom", "it broke".to_owned());
        assert_eq!(
            recorded_frame(&session, 2),
            r#"{"code":"init.boom","message":"it broke","seq":2,"session_id":"init_golden_error","type":"error"}"#
        );
        assert_eq!(
            session.error_replay_frame().expect("error replay frame"),
            r#"{"type":"error","session_id":"init_golden_error","code":"init.boom","message":"it broke"}"#
        );
        session.ack_error().expect("ack error");
        assert_eq!(
            recorded_frame(&session, 3),
            r#"{"seq":3,"session_id":"init_golden_error","status":"errored","type":"error_acked"}"#
        );
    }

    #[test]
    fn golden_error_expired_event_bytes() {
        let session = test_session("init_golden_expired");
        session.set_error("init.boom", "it broke".to_owned());
        session.expire("error_ack_timeout");
        assert_eq!(
            recorded_frame(&session, 3),
            r#"{"reason":"error_ack_timeout","seq":3,"session_id":"init_golden_expired","type":"error_expired"}"#
        );
    }

    #[test]
    fn golden_hello_frame_bytes() {
        // `state` sits beside `status`: the two answer the same question at
        // different resolutions, and the snapshot is declaration-ordered
        // (`current_step` then `categories`) unlike the sorted state event.
        let session = test_session("init_golden_hello");
        assert_eq!(
            session.hello_frame(),
            format!(
                r#"{{"type":"hello","session_id":"init_golden_hello","status":"running","state":{FRESH_STATE_JSON},"last_seq":1,"pending_input":null,"result_available":false,"error":null}}"#
            )
        );
        // The errored hello pins the nested `PublicError` object, the one
        // part of the frame that goes through a `Serialize` impl.
        session.set_error("init.boom", "it broke".to_owned());
        assert_eq!(
            session.hello_frame(),
            format!(
                r#"{{"type":"hello","session_id":"init_golden_hello","status":"errored","state":{FRESH_STATE_JSON},"last_seq":2,"pending_input":null,"result_available":false,"error":{{"code":"init.boom","message":"it broke"}}}}"#
            )
        );
    }

    /// The snapshot of a session no signal has reached yet: nothing is settled,
    /// the four root categories are ready, and everything else is blocked on
    /// the dependency table.
    const FRESH_STATE_JSON: &str = r#"{"current_step":null,"categories":[{"id":"agent","status":"ready"},{"id":"provider","status":"blocked","blocked_on":"agent"},{"id":"model","status":"blocked","blocked_on":"provider"},{"id":"mode","status":"blocked","blocked_on":"model"},{"id":"workspace","status":"ready"},{"id":"native_config","status":"ready"},{"id":"mcp","status":"blocked","blocked_on":"agent"},{"id":"skills","status":"blocked","blocked_on":"agent"},{"id":"deps","status":"ready"}]}"#;

    #[test]
    fn golden_hello_frame_with_pending_input_and_result_bytes() {
        // The reconnect cases: a hello sent while the wizard is blocked on a
        // prompt, and one sent after the result is waiting to be acked. Both
        // populate fields the fresh-session hello leaves null.
        let session = test_session("init_golden_hello_pending");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = hosted_test_request(
            HostedPromptKind::Model,
            HostedPromptStyle::Text,
            "model",
            &[],
        );
        let handle = std::thread::spawn(move || driver.text(request));
        let pending = wait_for_pending_input(&session);
        // The pending prompt is what makes `model` derive as awaiting_input,
        // and it is the only category that may: there is one prompt slot.
        assert_eq!(
            session.hello_frame(),
            format!(
                r#"{{"type":"hello","session_id":"init_golden_hello_pending","status":"waiting_for_input","state":{{"current_step":null,"categories":[{{"id":"agent","status":"ready"}},{{"id":"provider","status":"blocked","blocked_on":"agent"}},{{"id":"model","status":"awaiting_input"}},{{"id":"mode","status":"blocked","blocked_on":"model"}},{{"id":"workspace","status":"ready"}},{{"id":"native_config","status":"ready"}},{{"id":"mcp","status":"blocked","blocked_on":"agent"}},{{"id":"skills","status":"blocked","blocked_on":"agent"}},{{"id":"deps","status":"ready"}}]}},"last_seq":3,"pending_input":{{"request_id":"{}","kind":"model","style":"text","prompt":"model","required":false,"default":null,"options":[]}},"result_available":false,"error":null}}"#,
                pending.request_id
            )
        );
        session
            .submit_input(&pending.request_id, json!("gpt-5"))
            .expect("submit input");
        handle
            .join()
            .expect("driver thread")
            .expect("driver result");

        session.set_result(json!({"status": "initialized"}));
        // The answered prompt released the frontier, so the completed hello is
        // back to the fresh snapshot: an answer settles nothing on its own.
        assert_eq!(
            session.hello_frame(),
            format!(
                r#"{{"type":"hello","session_id":"init_golden_hello_pending","status":"completed_awaiting_ack","state":{FRESH_STATE_JSON},"last_seq":6,"pending_input":null,"result_available":true,"error":null}}"#
            )
        );
    }

    #[test]
    fn golden_ack_accepted_and_error_acked_close_frame_bytes() {
        let session = test_session("init_golden_close");
        session.set_result(json!({"status": "initialized"}));
        let ClientFrameOutcome::Close(frame) =
            handle_client_frame(&session, r#"{"type":"ack_result"}"#)
        else {
            panic!("ack_result must close the connection");
        };
        assert_eq!(
            frame,
            r#"{"type":"ack_accepted","session_id":"init_golden_close"}"#
        );

        let errored = test_session("init_golden_close_error");
        errored.set_error("init.boom", "it broke".to_owned());
        let ClientFrameOutcome::Close(frame) =
            handle_client_frame(&errored, r#"{"type":"ack_error"}"#)
        else {
            panic!("ack_error must close the connection");
        };
        assert_eq!(
            frame,
            r#"{"type":"error_acked","session_id":"init_golden_close_error"}"#
        );
    }

    #[test]
    fn golden_protocol_error_frame_bytes() {
        let session = test_session("init_golden_protocol");
        let sent = |text: &str| match handle_client_frame(&session, text) {
            ClientFrameOutcome::Send(frame) => frame,
            _ => panic!("frame `{text}` must produce a Send outcome"),
        };
        assert_eq!(
            sent("not json"),
            r#"{"type":"error","code":"init.bad_frame","message":"invalid client frame: expected ident at line 1 column 2"}"#
        );
        assert_eq!(
            sent(r#"{"type":"teleport"}"#),
            r#"{"type":"error","code":"init.unsupported_frame","message":"unsupported client frame `teleport`"}"#
        );
        assert_eq!(
            sent(r#"{"type":"input"}"#),
            r#"{"type":"error","code":"init.missing_request_id","message":"input frame requires request_id"}"#
        );
        assert_eq!(
            sent(r#"{"type":"input","request_id":"ireq_stale"}"#),
            r#"{"type":"error","code":"init.input_rejected","message":"no input request is pending"}"#
        );
        assert_eq!(
            sent(r#"{"type":"replay_result"}"#),
            r#"{"type":"error","code":"init.result_unavailable","message":"init result is not available"}"#
        );
        assert_eq!(
            sent(r#"{"type":"replay_error"}"#),
            r#"{"type":"error","code":"init.error_unavailable","message":"no init error is recorded for this session"}"#
        );
        assert_eq!(
            sent(r#"{"type":"ack_result"}"#),
            r#"{"type":"error","code":"init.ack_rejected","message":"no init result is awaiting acknowledgement"}"#
        );
        assert_eq!(
            ws_lagged_frame(),
            r#"{"type":"error","code":"init.ws_lagged","message":"websocket client lagged behind init event stream"}"#
        );
    }

    #[test]
    fn golden_encode_failure_frame_is_valid_json() {
        // This frame is spliced from constants instead of serialized, which is
        // what makes it the one frame that cannot itself fail to encode. That
        // only holds while neither constant contains a JSON metacharacter, so
        // the round-trip is asserted rather than assumed.
        let frame = encode_failure_frame();
        assert_eq!(
            frame,
            r#"{"type":"error","code":"init.frame_encode_failed","message":"init frame payload could not be encoded"}"#
        );
        let parsed: Value = serde_json::from_str(&frame).expect("encode-failure frame must parse");
        assert_eq!(parsed["code"], json!(FRAME_ENCODE_FAILED_CODE));
        assert_eq!(parsed["message"], json!(FRAME_ENCODE_FAILED_MESSAGE));
    }

    #[test]
    fn serde_json_map_is_insertion_ordered() {
        // A canary, not a preference: `agent-client-protocol` turns on
        // `serde_json/preserve_order` for the whole build, which is why
        // seq-bearing events are assembled through a `BTreeMap` and seq-less
        // frames through derived structs. If a dependency change ever drops
        // the feature, this fails loudly instead of quietly re-sorting keys
        // the golden pins above cannot all observe.
        assert_eq!(json!({"b": 1, "a": 2}).to_string(), r#"{"b":1,"a":2}"#);
    }
}
