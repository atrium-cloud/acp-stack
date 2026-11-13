//! Generic projection of the whole advertised ACP session config option set
//! into one wire/storage shape, backing the config-option routes and snapshots.

use agent_client_protocol::v1::schema::{
    SessionConfigKind, SessionConfigOption, SessionConfigSelectOption, SessionConfigSelectOptions,
};
use serde::{Deserialize, Serialize};

/// One advertised config option, as served over the API and stored in the
/// per-session snapshot. `_meta` is deliberately dropped (agent-opaque and
/// unbounded); the verbatim `session.update` events remain the source of
/// truth for full payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionConfigOptionSnapshot {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The ACP category verbatim (`mode`, `model`, `model_config`,
    /// `thought_level`, a `_`-prefixed custom, or a future reserved value);
    /// absent when the agent advertised none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Option kind: `"select"` or `"boolean"`.
    #[serde(rename = "type")]
    #[schemars(extend("enum" = ["select", "boolean"]))]
    pub kind: String,
    pub current_value: SessionConfigOptionSnapshotValue,
    /// Select choices in advertised order, flattened across groups, a grouped
    /// choice carrying its `group`. Absent for boolean options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<SessionConfigOptionChoice>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionConfigOptionChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The group the agent advertised this choice under; absent when ungrouped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<SessionConfigOptionGroup>,
}

/// A select choice group's id and name as the agent advertised them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionConfigOptionGroup {
    pub id: String,
    pub name: String,
}

/// `Bool` first so JSON `true` never deserializes as the string `"true"`;
/// mirrors `AgentConfigOptionValue`'s ordering constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum SessionConfigOptionSnapshotValue {
    Bool(bool),
    Text(String),
}

pub const SNAPSHOT_KIND_SELECT: &str = "select";
pub const SNAPSHOT_KIND_BOOLEAN: &str = "boolean";

/// Project an advertised option list. Options whose kind this runtime cannot
/// encode (a future `SessionConfigKind` variant) are skipped with a warning:
/// a client cannot set what we cannot represent, and surfacing them with a
/// bogus type would invite exactly that.
pub fn project_config_options(options: &[SessionConfigOption]) -> Vec<SessionConfigOptionSnapshot> {
    options.iter().filter_map(project_config_option).collect()
}

fn project_config_option(option: &SessionConfigOption) -> Option<SessionConfigOptionSnapshot> {
    let category =
        option
            .category
            .as_ref()
            .and_then(|category| match serde_json::to_value(category) {
                Ok(serde_json::Value::String(value)) => Some(value),
                _ => None,
            });
    let (kind, current_value, choices) = match &option.kind {
        SessionConfigKind::Select(select) => {
            let Some(choices) = project_select_choices(&select.options) else {
                tracing::warn!(
                    option = %option.id.0,
                    options = ?select.options,
                    "skipping select config option with unencodable choices"
                );
                return None;
            };
            (
                SNAPSHOT_KIND_SELECT,
                SessionConfigOptionSnapshotValue::Text(select.current_value.0.to_string()),
                Some(choices),
            )
        }
        SessionConfigKind::Boolean(boolean) => (
            SNAPSHOT_KIND_BOOLEAN,
            SessionConfigOptionSnapshotValue::Bool(boolean.current_value),
            None,
        ),
        other => {
            tracing::warn!(
                option = %option.id.0,
                ?other,
                "skipping config option with unencodable kind"
            );
            return None;
        }
    };
    Some(SessionConfigOptionSnapshot {
        id: option.id.0.to_string(),
        name: option.name.clone(),
        description: option.description.clone(),
        category,
        kind: kind.to_owned(),
        current_value,
        options: choices,
    })
}

/// Select choices in advertised order, flattened across groups. `None` for a
/// choice shape this runtime cannot encode (a future `SessionConfigSelectOptions` variant).
pub(crate) fn project_select_choices(
    options: &SessionConfigSelectOptions,
) -> Option<Vec<SessionConfigOptionChoice>> {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => Some(
            options
                .iter()
                .map(|choice| project_choice(choice, None))
                .collect(),
        ),
        SessionConfigSelectOptions::Grouped(groups) => Some(
            groups
                .iter()
                .flat_map(|group| {
                    let projected = SessionConfigOptionGroup {
                        id: group.group.0.to_string(),
                        name: group.name.clone(),
                    };
                    group
                        .options
                        .iter()
                        .map(move |choice| project_choice(choice, Some(projected.clone())))
                })
                .collect(),
        ),
        _ => None,
    }
}

fn project_choice(
    choice: &SessionConfigSelectOption,
    group: Option<SessionConfigOptionGroup>,
) -> SessionConfigOptionChoice {
    SessionConfigOptionChoice {
        value: choice.value.0.to_string(),
        name: choice.name.clone(),
        description: choice.description.clone(),
        group,
    }
}
