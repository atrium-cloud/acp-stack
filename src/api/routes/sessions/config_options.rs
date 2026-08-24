use super::*;

use crate::runtime::agent::config_options::{
    SNAPSHOT_KIND_BOOLEAN, SNAPSHOT_KIND_SELECT, SessionConfigOptionSnapshot,
};
use crate::state::{SESSION_METADATA_CONFIG_OPTIONS, SESSION_METADATA_CONFIG_OPTIONS_UPDATED_AT};

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SessionConfigOptionsResponse {
    /// The session's config options as last observed: seeded from
    /// `session/new` at create, refreshed by `session/set_config_option`
    /// responses and `config_option_update` notifications. Empty when no
    /// snapshot has been stored (sessions created before the feature).
    config_options: Vec<SessionConfigOptionSnapshot>,
    /// When the stored snapshot was last replaced. `null` when none exists.
    updated_at: Option<String>,
}

pub(crate) async fn sessions_config_options_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
) -> std::result::Result<ApiSuccess<SessionConfigOptionsResponse>, StackError> {
    let store = state.state.lock().await;
    let session = store
        .get_session(&id)?
        .ok_or_else(|| StackError::SessionNotFound { id: id.clone() })?;
    drop(store);
    if let Some(asserted) = params.target_id.as_deref()
        && asserted != session.target_id
    {
        return Err(StackError::InvalidParam {
            field: "target",
            reason: format!(
                "session `{}` belongs to target `{}`, not `{asserted}`",
                session.id, session.target_id
            ),
        });
    }
    let stored = stored_config_options(&session.metadata_json);
    let (config_options, updated_at) = stored
        .map(|stored| (stored.options, stored.updated_at))
        .unwrap_or_default();
    Ok(ApiSuccess::new(SessionConfigOptionsResponse {
        config_options,
        updated_at,
    }))
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SessionConfigOptionSetBody {
    /// The advertised option id to set.
    config_id: String,
    /// The value to apply: a string for select options, a boolean for
    /// boolean options.
    value: SessionConfigOptionSetValue,
}

/// `Bool` first so JSON `true` never deserializes as the string `"true"`.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum SessionConfigOptionSetValue {
    Bool(bool),
    Text(String),
}

pub(crate) async fn sessions_config_options_set_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SessionsTargetParams>,
    Json(payload): Json<SessionConfigOptionSetBody>,
) -> std::result::Result<ApiSuccess<SessionConfigOptionsResponse>, StackError> {
    let config_id = payload.config_id.trim();
    if config_id.is_empty() {
        return Err(StackError::InvalidParam {
            field: "config_id",
            reason: "config_id must not be empty".to_owned(),
        });
    }
    let target = target_for_existing_session(&state, &id, params.target_id.as_deref()).await?;

    // Pre-validate against the stored snapshot so an off-list request gets a retryable 400.
    // An empty snapshot skips the check: it is observational, not authoritative.
    let session = {
        let store = state.state.lock().await;
        store
            .get_session(&id)?
            .ok_or_else(|| StackError::SessionNotFound { id: id.clone() })?
    };
    if let Some(stored) = stored_config_options(&session.metadata_json)
        && !stored.options.is_empty()
    {
        let advertised = stored
            .options
            .iter()
            .find(|option| option.id == config_id)
            .ok_or_else(|| StackError::InvalidParam {
                field: "config_id",
                reason: format!("session does not advertise config option `{config_id}`"),
            })?;
        match (&payload.value, advertised.kind.as_str()) {
            (SessionConfigOptionSetValue::Bool(_), SNAPSHOT_KIND_BOOLEAN) => {}
            (SessionConfigOptionSetValue::Text(text), SNAPSHOT_KIND_SELECT) => {
                let known = advertised
                    .options
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|choice| choice.value == *text);
                if !known {
                    return Err(StackError::InvalidParam {
                        field: "value",
                        reason: format!(
                            "`{text}` is not an advertised value of config option `{config_id}`"
                        ),
                    });
                }
            }
            (_, kind) => {
                return Err(StackError::InvalidParam {
                    field: "value",
                    reason: format!(
                        "config option `{config_id}` is a {kind} option; send a matching value type"
                    ),
                });
            }
        }
    }

    ensure_agent_started(&state, &target.target_id).await?;
    let value = match payload.value {
        SessionConfigOptionSetValue::Bool(value) => {
            agent_client_protocol::schema::v1::SessionConfigOptionValue::Boolean { value }
        }
        SessionConfigOptionSetValue::Text(text) => {
            agent_client_protocol::schema::v1::SessionConfigOptionValue::ValueId {
                value: agent_client_protocol::schema::v1::SessionConfigValueId::new(text),
            }
        }
    };
    let config_options = target
        .supervisor
        .set_session_config_option(&id, config_id, value, &state.state)
        .await?;
    // Re-read so the response matches a follow-up GET: when a lax adapter answers with an
    // empty list and refreshes by notification, the stored snapshot is the truthful one.
    let stored = {
        let store = state.state.lock().await;
        store
            .get_session(&id)?
            .and_then(|session| stored_config_options(&session.metadata_json))
    };
    let (config_options, updated_at) = match stored {
        Some(stored) if config_options.is_empty() => (stored.options, stored.updated_at),
        Some(stored) => (config_options, stored.updated_at),
        None => (config_options, None),
    };
    Ok(ApiSuccess::new(SessionConfigOptionsResponse {
        config_options,
        updated_at,
    }))
}

pub(crate) struct StoredConfigOptions {
    pub(crate) options: Vec<SessionConfigOptionSnapshot>,
    pub(crate) updated_at: Option<String>,
}

/// Read the stored config-option snapshot off a session's metadata. `None`
/// means no snapshot was ever stored; a malformed value degrades to `None`
/// with a warning instead of failing the caller.
pub(crate) fn stored_config_options(metadata_json: &str) -> Option<StoredConfigOptions> {
    let metadata = match serde_json::from_str::<serde_json::Value>(metadata_json) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "session metadata_json is not valid JSON");
            return None;
        }
    };
    let options = metadata.get(SESSION_METADATA_CONFIG_OPTIONS)?;
    let options = match serde_json::from_value::<Vec<SessionConfigOptionSnapshot>>(options.clone())
    {
        Ok(options) => options,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "stored session config_options snapshot has an unexpected shape"
            );
            return None;
        }
    };
    let updated_at = metadata
        .get(SESSION_METADATA_CONFIG_OPTIONS_UPDATED_AT)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some(StoredConfigOptions {
        options,
        updated_at,
    })
}
