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

use crate::api::core::load_runtime_config_from_disk;
use crate::api::routes::providers::{
    ModelsParams, models_response_for_config, resolve_models_target_config,
};
use crate::auth::constant_time_eq;
use crate::config;
use crate::envelope::{ApiError, ApiSuccess};
use crate::error::{Result, StackError};
use crate::extensions::managed_state::ApplyResponse;
use crate::fs_util::{acquire_agent_config_mutation_file_lock, home_dir};
use crate::runtime::agent::native_config_import::{NativeConfigInspection, NativeConfigSelection};
use crate::runtime::init_runner::StepDisposition;
#[cfg(test)]
use crate::runtime::init_runner::step_kind;
use crate::secrets::{SharedSecretStore, lock_shared_secret_store};
use crate::state::default_state_path;

use super::prompt::{
    self, ConfirmAnswer, HostedPromptDriver, HostedPromptKind, HostedPromptOutcome,
    HostedPromptRequest, HostedPromptStyle,
};
use super::state_signal::InitStateSignal;
#[cfg(test)]
use super::state_signal::{ApplicabilitySource, InitCategory, category_for_step_kind};
use super::{
    InitArgs, InitMcpHttpHeader, InitMcpHttpServer, InitMcpStdioServer, InitMode,
    InitNativeConfigUpload, run_hosted_init,
};

mod frames;
mod prompt_driver;
mod reaper;
mod request_dto;
mod response_dto;
mod routes;
mod session;

// The `pub(crate) use` below lifts only the def-producing functions up to
// `crate::cli` so `schema_export` reaches them without exposing the DTOs.
#[cfg(feature = "dev-tools")]
mod schema_umbrella;
#[cfg(feature = "dev-tools")]
pub(crate) use self::schema_umbrella::{init_request_defs, init_response_defs};

// Plain (non-re-exporting) globs keep each sibling's `pub(super)` items private
// to this parent, so nothing escapes `serve` beyond `run_init_serve`.
use self::frames::*;
use self::prompt_driver::*;
use self::reaper::*;
use self::request_dto::*;
use self::response_dto::*;
use self::routes::*;
use self::session::*;

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
        // The serve process's single writer-visible store handle, opened before
        // any session so the wizard thread and the credential-deposit route
        // share it: a second decrypted snapshot's whole-file persist would
        // clobber the other writer's mutations.
        let secret_store = crate::secrets::new_shared_secret_store(
            crate::secrets::SecretStore::open_or_create(&home_dir()?)?,
        );
        let state = BootstrapState {
            token: Arc::new(token),
            allowed_origins: Arc::new(args.allowed_origin),
            manager: HostedInitManager::new(secret_store.clone()),
            native_config_mutation: Arc::new(TokioMutex::new(())),
            secret_store,
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

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
