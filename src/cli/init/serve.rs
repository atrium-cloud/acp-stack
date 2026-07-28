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
use serde_json::{Value, json};
use tokio::sync::{Mutex as TokioMutex, Notify, broadcast};
use tower_http::limit::RequestBodyLimitLayer;
use zeroize::{Zeroize, Zeroizing};

use crate::auth::constant_time_eq;
use crate::config;
use crate::envelope::{ApiError, ApiSuccess};
use crate::error::{Result, StackError};
use crate::fs_util::{acquire_agent_config_mutation_file_lock, home_dir};
use crate::runtime::agent::native_config_import::{NativeConfigInspection, NativeConfigSelection};
use crate::state::default_state_path;

use super::prompt::{
    self, HostedPromptDriver, HostedPromptOutcome, HostedPromptRequest, HostedPromptStyle,
};
use super::{
    CloudflareModeArg, CloudflaredDeploymentArg, InitArgs, InitMode, InitNativeConfigUpload,
    run_hosted_init,
};

mod prompt_driver;
mod routes;
mod session;

// Plain (non-re-exporting) globs make each sibling's `pub(super)` items private
// members of this parent module, so the other siblings and the `tests` module
// reach them via `super::NAME` / `super::*`. Nothing here escapes `serve`
// beyond `run_init_serve`/`InitServeArgs`, which stay defined in this parent.
use self::prompt_driver::*;
use self::routes::*;
use self::session::*;

const DEFAULT_INIT_TOKEN_ENV: &str = "ACP_STACK_INIT_TOKEN";
const INIT_BOOTSTRAP_TOKEN_FIELD: &str = "bootstrap token";
const INIT_WS_CHANNEL_CAPACITY: usize = 128;
const INIT_EVENT_HISTORY_LIMIT: usize = 256;
const DEFAULT_INIT_IDLE_TIMEOUT: &str = "15m";
const IDLE_REAPER_TICK: std::time::Duration = std::time::Duration::from_secs(1);

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
        if let Some(timeout) = idle_timeout {
            tokio::spawn(reap_idle_session(shutdown_manager.clone(), timeout));
        }
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
async fn reap_idle_session(manager: Arc<HostedInitManager>, timeout: std::time::Duration) {
    loop {
        tokio::time::sleep(IDLE_REAPER_TICK).await;
        match manager.session_current() {
            Some(session) => {
                if !session.has_connected_ws()
                    && session.last_activity_age_secs() >= timeout.as_secs()
                {
                    session.expire("idle_timeout");
                    break;
                }
            }
            None => {
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
    provider: Option<String>,
    api_key_ref: Option<String>,
    model: Option<String>,
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeConfigUploadRequest {
    filename: String,
    content: String,
}

impl StartInitRequest {
    fn into_init_args(self) -> InitArgs {
        let mut args = empty_init_args();
        args.agent = self.agent;
        args.provider = self.provider;
        args.api_key_ref = self.api_key_ref;
        args.model = self.model;
        args.custom_provider = self.custom_provider.unwrap_or(false);
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
        args
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

    use axum::body::to_bytes;
    use http::{Method, Request};
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

    fn hosted_items(labels: &[&str]) -> Vec<(usize, String, String)> {
        labels
            .iter()
            .enumerate()
            .map(|(index, label)| (index, (*label).to_owned(), String::new()))
            .collect()
    }

    fn hosted_test_request(
        style: HostedPromptStyle,
        prompt: &str,
        labels: &[&str],
    ) -> HostedPromptRequest {
        HostedPromptRequest {
            style,
            prompt: prompt.to_owned(),
            required: false,
            default: None,
            items: hosted_items(labels)
                .into_iter()
                .map(|(_, label, hint)| prompt::HostedPromptItem { label, hint })
                .collect(),
            inspection: None,
        }
    }

    fn send_select_response(
        prompt: &str,
        labels: &[&str],
        response: Value,
    ) -> HostedPromptOutcome<Option<usize>> {
        let session = test_session("init_driver_select");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = hosted_test_request(HostedPromptStyle::SearchableSelect, prompt, labels);
        let handle = std::thread::spawn(move || driver.select(request));
        let pending = wait_for_pending_input(&session);
        session
            .submit_input(&pending.request_id, response)
            .expect("submit input");
        handle
            .join()
            .expect("driver thread")
            .expect("driver result")
    }

    #[test]
    fn start_init_request_maps_sandbox_into_args() {
        // `deny_unknown_fields` means the platform payload is rejected outright
        // unless `sandbox` is a known field; this also covers the arg mapping.
        let request: StartInitRequest =
            serde_json::from_str(r#"{"agent":"placebo","sandbox":"unshare"}"#)
                .expect("sandbox must be an accepted request field");
        let args = request.into_init_args();
        assert_eq!(args.sandbox.as_deref(), Some("unshare"));
    }

    #[test]
    fn hosted_driver_accepts_provider_password_and_model_responses() {
        let provider = send_select_response(
            "provider for opencode",
            &["OpenRouter (openrouter)", "DeepSeek (deepseek)"],
            json!("OpenRouter (openrouter)"),
        );
        assert_eq!(provider, HostedPromptOutcome::Handled(Some(0)));

        let model = send_select_response(
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
            style: HostedPromptStyle::NativeConfigReview,
            prompt: "Review native Agent config".to_owned(),
            required: true,
            default: None,
            items: Vec::new(),
            inspection: Some(inspection),
        };
        let handle = std::thread::spawn(move || driver.native_config_review(request));
        let pending = wait_for_pending_input(&session);
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

    #[test]
    fn stale_input_request_id_is_rejected() {
        let session = test_session("init_stale_input");
        let driver = SessionPromptDriver {
            session: session.clone(),
        };
        let request = HostedPromptRequest {
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
    async fn error_notifies_terminal_waiter_and_returns_failure() {
        let manager = HostedInitManager::new();
        let session = HostedInitSession::new("init_error".to_owned(), manager.shutdown.clone());
        *lock_unpoisoned(&manager.active) = Some(session.clone());

        {
            let waiter = manager.wait_for_terminal();
            tokio::pin!(waiter);
            session.set_error("init.failed", "provider setup failed".to_owned());

            tokio::time::timeout(Duration::from_secs(1), &mut waiter)
                .await
                .expect("terminal waiter should be notified");
        }
        let error = manager
            .terminal_result()
            .expect_err("errored session should return failure");
        assert!(
            error
                .public_message()
                .contains("init.failed: provider setup failed")
        );
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
        session.push_event("progress", json!({ "message": "first" }));
        session.push_event("progress", json!({ "message": "second" }));
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
            std::time::Duration::from_secs(10),
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
            std::time::Duration::from_secs(10),
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
            std::time::Duration::from_secs(10),
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
            std::time::Duration::from_secs(10),
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
            std::time::Duration::from_secs(10),
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
            std::time::Duration::from_secs(10),
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
}
