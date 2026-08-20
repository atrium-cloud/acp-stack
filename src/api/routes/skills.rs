//! `/v1/agent/skills` — day-2 Agent Skills management for the active agent.
//!
//! Reads are session-tier: `list` (installed skills), `catalog` (built-in
//! catalog plus configured user sources), and `source` (live inspection of one
//! source's offered skills). Mutations are admin-tier and declared in the admin
//! sub-router: `add`/`remove` install and uninstall skills, and
//! `sources/add`/`sources/remove` register and drop `[[skills.sources]]`
//! entries in config.
//!
//! `add` and `source` download and extract a GitHub archive off the async
//! runtime; `add` fetches *before* taking the config-mutation lock so a slow
//! download cannot block `agent switch`, then holds the lock only for the copy,
//! re-resolving the active agent under it in case a switch landed mid-fetch.
//! `remove` validates input before locking and holds the lock only for the
//! delete. Both refresh the harness symlink mirror after the lock is released.
//! The `sources/*` mutations take the same lock to serialize config writes and
//! validate before writing atomically.
//!
//! Every handler loads config leniently, dropping individually invalid
//! declarations the same way daemon boot does: one bad hand-edited
//! `[[skills.sources]]` entry must not 400 the whole skills surface —
//! `sources/remove` in particular is the route that repairs it. A `sources/*`
//! write canonicalizes that lenient view back to disk, which heals dropped
//! entries out of the file; each healed entry is warned about at write time.

use std::path::PathBuf;

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use super::super::core::{AppState, load_active_registry};
use crate::config::{Config, DEFAULT_SKILL_SOURCE_BRANCH, UserSkillSource};
use crate::envelope::ApiSuccess;
use crate::error::StackError;
use crate::fs_util::{atomic_write_owner_only, home_dir};
use crate::runtime::install::agent_registry::RegistryEntry;
use crate::runtime::install::skill_installer::{
    InstalledSkill, SkillInstallReport, SkillLinkReport, SkillMetadata, SkillRemoveReport,
    expand_agent_skills_install_dir, fetch_and_extract_source, inspect_source,
    install_from_extracted_root, link_agent_skills_best_effort, list_installed_skills,
    parse_skill_names, remove_agent_skill, resolve_source_ref, validate_install_target_name,
    validate_requested_skills,
};
use crate::runtime::install::skill_registry::SkillCatalog;

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SkillsListResponse {
    agent_id: String,
    /// Whether the active agent is a managed Agent Skills install target.
    /// When false, `skills` is always empty and `install_dir` is absent.
    supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_dir: Option<PathBuf>,
    skills: Vec<InstalledSkill>,
}

pub(crate) async fn skills_list_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SkillsListResponse>, StackError> {
    let home = home_dir()?;
    let config = Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
    let registry = load_active_registry()?;
    let entry = registry.lookup_required(&config.agent.id)?;
    let install_dir = agent_install_dir(entry);
    let skills = list_installed_skills(&home, entry)?;
    Ok(ApiSuccess::new(SkillsListResponse {
        agent_id: config.agent.id.clone(),
        supported: install_dir.is_some(),
        install_dir: install_dir
            .map(|dir| expand_agent_skills_install_dir(&home, dir))
            .transpose()?,
        skills,
    }))
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SkillsCatalogResponse {
    sources: Vec<SkillCatalogSourceJson>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SkillCatalogSourceJson {
    id: String,
    alias: String,
    name: String,
    /// `owner/repo`, the GitHub source of the skills.
    repo: String,
    /// True for the embedded curated catalog; false for user-declared sources.
    catalog: bool,
    trusted: bool,
    /// Selectors accepted by `add` (indexed catalog sources only; empty for
    /// user sources — use `source get` to enumerate those live).
    skills: Vec<String>,
    /// Subset of `skills` installed by the Standard Setup essentials step.
    essential: Vec<String>,
}

pub(crate) async fn skills_catalog_handler(
    State(state): State<AppState>,
) -> std::result::Result<ApiSuccess<SkillsCatalogResponse>, StackError> {
    let config = Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
    let catalog = SkillCatalog::load_embedded()?;
    let mut sources = catalog
        .sources()
        .iter()
        .map(|source| SkillCatalogSourceJson {
            id: source.id.clone(),
            alias: source.alias.clone(),
            name: source.name.clone(),
            repo: format!("{}/{}", source.owner, source.repo),
            catalog: true,
            trusted: source.trusted,
            skills: source
                .indexed_skills
                .iter()
                .map(|skill| skill.selector.clone())
                .collect(),
            essential: source.essential_skills.clone(),
        })
        .collect::<Vec<_>>();
    for user in &config.skills.sources {
        sources.push(SkillCatalogSourceJson {
            id: user.alias.clone(),
            alias: user.alias.clone(),
            name: format!("{} (user source)", user.alias),
            repo: user.github.clone(),
            catalog: false,
            trusted: user.trusted,
            skills: Vec::new(),
            essential: Vec::new(),
        });
    }
    Ok(ApiSuccess::new(SkillsCatalogResponse { sources }))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SkillsAddRequest {
    /// Catalog alias, a configured user alias, or `github:<owner>[/<repo>]`.
    source: String,
    /// Skill selectors; comma-separated values in an entry are also accepted.
    #[serde(default)]
    skills: Vec<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SkillsAddResponse {
    agent_id: String,
    install: SkillInstallReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_link: Option<SkillLinkReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_link_error: Option<String>,
}

pub(crate) async fn skills_add_handler(
    State(state): State<AppState>,
    Json(body): Json<SkillsAddRequest>,
) -> std::result::Result<ApiSuccess<SkillsAddResponse>, StackError> {
    let home = home_dir()?;
    let config = Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
    let registry = load_active_registry()?;
    let entry = registry.lookup_required(&config.agent.id)?;
    // Fail fast before spending a download; the install destination itself is
    // re-resolved under the mutation lock below.
    if agent_install_dir(entry).is_none() {
        return Err(unsupported_skills_agent_error(&config.agent.id));
    }
    let catalog = SkillCatalog::load_embedded()?;
    let source = resolve_source_ref(&body.source, &config.skills.sources, &catalog)?;
    if body.skills.is_empty() {
        return Err(StackError::MissingField { field: "skills" });
    }
    let skills = parse_skill_names(&body.skills)?;
    // Reject unknown selectors before spending a download (mirrors the
    // fail-fast `install_from_github` did when it validated ahead of fetching).
    validate_requested_skills(&source, &skills)?;

    // Download + extract to a private tempdir *without* the config-mutation lock:
    // the fetch touches no shared state and can stall to the read timeout, which
    // must not block `agent switch` or other config writers. Blocking work, so
    // park it off the async runtime.
    let fetch_source = source.clone();
    let (archive, archive_root) =
        tokio::task::spawn_blocking(move || fetch_and_extract_source(&fetch_source))
            .await
            .map_err(|err| StackError::SkillInstallFailed {
                reason: format!("skill fetch thread join failed: {err}"),
            })??;

    // Only the copy into the shared skill dir must serialize with switch. An
    // `agent switch` may have landed during the fetch, so re-resolve the active
    // agent and its install dir under the lock rather than trusting the
    // pre-fetch config.
    let (agent_id, entry, install) = {
        let _mutation = state.lock_agent_config_mutation().await?;
        let config = Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
        let registry = load_active_registry()?;
        let entry = registry.lookup_required(&config.agent.id)?;
        let install_dir = agent_install_dir(entry)
            .ok_or_else(|| unsupported_skills_agent_error(&config.agent.id))?;
        let destination_root = expand_agent_skills_install_dir(&home, install_dir)?;
        let install = tokio::task::spawn_blocking(move || {
            let report =
                install_from_extracted_root(&source, &archive_root, &destination_root, &skills);
            // Hold the tempdir open until the copy finishes, then let it drop.
            drop(archive);
            report
        })
        .await
        .map_err(|err| StackError::SkillInstallFailed {
            reason: format!("skill install thread join failed: {err}"),
        })??;
        (config.agent.id.clone(), entry.clone(), install)
    };

    // The link refresh rewrites only the harness's discovery dir, not config,
    // so it runs after the mutation lock is released.
    let link_outcome = link_agent_skills_best_effort(&home, &entry);
    // Audit trail for "which skills has acp-stack installed": day-2 mutations
    // are recorded as events (init-time installs are already durable in the
    // agent_skills_install step payload). The payload carries no filesystem
    // paths — events are session-tier readable. A logging failure must not
    // fail an install that already happened.
    let installed: Vec<&str> = install
        .installed
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let skipped: Vec<&str> = install
        .skipped
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    record_skill_event(
        &state,
        "skill.install",
        serde_json::json!({
            "agent_id": agent_id,
            "source": body.source,
            "installed": installed,
            "skipped": skipped,
        }),
    )
    .await;
    Ok(ApiSuccess::new(SkillsAddResponse {
        agent_id,
        install,
        skills_link: link_outcome.report,
        skills_link_error: link_outcome.error,
    }))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SkillsRemoveRequest {
    /// Install name of the skill to remove (a `/`-joined path for nested
    /// skills, e.g. `zoom/android`).
    skill: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SkillsRemoveResponse {
    agent_id: String,
    remove: SkillRemoveReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_link: Option<SkillLinkReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_link_error: Option<String>,
}

pub(crate) async fn skills_remove_handler(
    State(state): State<AppState>,
    Json(body): Json<SkillsRemoveRequest>,
) -> std::result::Result<ApiSuccess<SkillsRemoveResponse>, StackError> {
    // Validate before taking the lock so malformed requests don't serialize
    // with `agent switch`. Bad client input maps to a 400 here; the installer
    // keeps its own `SkillInstallFailed` for the same check deeper down.
    validate_install_target_name(&body.skill).map_err(|_| StackError::InvalidParam {
        field: "skill",
        reason: format!("`{}` is not a valid skill install name", body.skill),
    })?;
    let home = home_dir()?;
    let config = Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
    let registry = load_active_registry()?;
    let entry = registry.lookup_required(&config.agent.id)?;
    if agent_install_dir(entry).is_none() {
        return Err(unsupported_skills_agent_error(&config.agent.id));
    }
    let (agent_id, entry, remove) = {
        let _mutation = state.lock_agent_config_mutation().await?;
        // Re-check under the lock: the active agent may have changed since the
        // pre-check above.
        let config = Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
        let registry = load_active_registry()?;
        let entry = registry.lookup_required(&config.agent.id)?;
        if agent_install_dir(entry).is_none() {
            return Err(unsupported_skills_agent_error(&config.agent.id));
        }
        let remove = remove_agent_skill(&home, entry, &body.skill)?;
        (config.agent.id.clone(), entry.clone(), remove)
    };
    let link_outcome = link_agent_skills_best_effort(&home, &entry);
    record_skill_event(
        &state,
        "skill.remove",
        serde_json::json!({
            "agent_id": agent_id,
            "skill": body.skill,
        }),
    )
    .await;
    Ok(ApiSuccess::new(SkillsRemoveResponse {
        agent_id,
        remove,
        skills_link: link_outcome.report,
        skills_link_error: link_outcome.error,
    }))
}

/// Best-effort audit record for day-2 skill mutations. Failures are logged,
/// never propagated: the filesystem change has already happened and a logging
/// outage must not turn a successful mutation into an error response.
async fn record_skill_event(state: &AppState, kind: &str, payload: serde_json::Value) {
    let payload_json = match serde_json::to_string(&payload) {
        Ok(payload_json) => payload_json,
        Err(error) => {
            tracing::warn!(%error, kind, "failed to serialize skill event payload");
            return;
        }
    };
    let store = state.state.lock().await;
    if let Err(error) = store.append_event_with_source(
        "info",
        kind,
        crate::state::EVENT_SOURCE_API,
        "",
        &payload_json,
    ) {
        tracing::warn!(%error, kind, "failed to record skill event");
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SkillSourceGetQuery {
    /// Catalog alias, configured user alias, or `github:<owner>[/<repo>]`.
    source: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SkillSourceGetResponse {
    id: String,
    /// `owner/repo`.
    repo: String,
    branch: String,
    /// True for the curated catalog; false for user/ad-hoc sources.
    catalog: bool,
    trusted: bool,
    skills: Vec<SkillMetadata>,
}

pub(crate) async fn skills_source_get_handler(
    State(state): State<AppState>,
    Query(query): Query<SkillSourceGetQuery>,
) -> std::result::Result<ApiSuccess<SkillSourceGetResponse>, StackError> {
    let config = Config::load_lenient_from_path(&state.runtime_paths.config_path)?;
    let catalog = SkillCatalog::load_embedded()?;
    let source = resolve_source_ref(&query.source, &config.skills.sources, &catalog)?;
    // Trust follows whichever source `resolve_source_ref` actually picked.
    // Catalog wins over a colliding user alias, so a catalog resolution reports
    // the curated source's trust; otherwise fall back to the user source's flag.
    let trusted = if source.catalog_managed {
        catalog
            .lookup_alias(query.source.trim())
            .map(|catalog_source| catalog_source.trusted)
            .unwrap_or(true)
    } else {
        config
            .skills
            .sources
            .iter()
            .find(|user| user.alias == query.source.trim())
            .map(|user| user.trusted)
            .unwrap_or(false)
    };
    let id = source.id.clone();
    let repo = format!("{}/{}", source.owner, source.repo);
    let branch = source.branch.clone();
    let catalog_managed = source.catalog_managed;
    // Downloads and extracts the source archive, so run it off the async runtime.
    let skills = tokio::task::spawn_blocking(move || inspect_source(&source))
        .await
        .map_err(|err| StackError::SkillInstallFailed {
            reason: format!("skill source inspect thread join failed: {err}"),
        })??;
    Ok(ApiSuccess::new(SkillSourceGetResponse {
        id,
        repo,
        branch,
        catalog: catalog_managed,
        trusted,
        skills,
    }))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SkillSourceAddRequest {
    alias: String,
    /// GitHub source as `owner/repo`.
    github: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    trusted: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SkillSourceAddResponse {
    alias: String,
    github: String,
    branch: String,
    trusted: bool,
    /// Total configured user sources after the add.
    sources: usize,
}

pub(crate) async fn skills_source_add_handler(
    State(state): State<AppState>,
    Json(body): Json<SkillSourceAddRequest>,
) -> std::result::Result<ApiSuccess<SkillSourceAddResponse>, StackError> {
    let _mutation = state.lock_agent_config_mutation().await?;
    let (mut config, dropped) =
        Config::load_lenient_from_path_reporting(&state.runtime_paths.config_path)?;
    // Reject shadowing a curated catalog alias. This check needs the catalog,
    // which the config layer deliberately does not load, so it lives here.
    let catalog = SkillCatalog::load_embedded()?;
    let alias = body.alias.trim().to_owned();
    if catalog.lookup_alias(&alias).is_some() {
        return Err(StackError::InvalidParam {
            field: "alias",
            reason: format!("`{alias}` is a built-in catalog alias"),
        });
    }
    let source = UserSkillSource {
        alias,
        github: body.github.trim().to_owned(),
        branch: body
            .branch
            .map(|branch| branch.trim().to_owned())
            .filter(|branch| !branch.is_empty())
            .unwrap_or_else(|| DEFAULT_SKILL_SOURCE_BRANCH.to_owned()),
        trusted: body.trusted,
    };
    config.skills.sources.push(source.clone());
    // Canonicalize and reload so alias syntax/uniqueness and github shape are
    // fully validated (the config layer's own rules) before the file is written.
    let canonical = config.to_canonical_toml()?;
    crate::config::load_config_from_str(&canonical)?;
    warn_dropped_declarations_healed(&dropped);
    atomic_write_owner_only(&state.runtime_paths.config_path, canonical.as_bytes())?;
    record_skill_event(
        &state,
        "skill.source_add",
        serde_json::json!({
            "alias": &source.alias,
            "github": &source.github,
            "branch": &source.branch,
            "trusted": source.trusted,
        }),
    )
    .await;
    Ok(ApiSuccess::new(SkillSourceAddResponse {
        alias: source.alias,
        github: source.github,
        branch: source.branch,
        trusted: source.trusted,
        sources: config.skills.sources.len(),
    }))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SkillSourceRemoveRequest {
    alias: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct SkillSourceRemoveResponse {
    alias: String,
    /// Total configured user sources after the removal.
    sources: usize,
}

pub(crate) async fn skills_source_remove_handler(
    State(state): State<AppState>,
    Json(body): Json<SkillSourceRemoveRequest>,
) -> std::result::Result<ApiSuccess<SkillSourceRemoveResponse>, StackError> {
    let _mutation = state.lock_agent_config_mutation().await?;
    let (mut config, dropped) =
        Config::load_lenient_from_path_reporting(&state.runtime_paths.config_path)?;
    let alias = body.alias.trim().to_owned();
    let before = config.skills.sources.len();
    config.skills.sources.retain(|source| source.alias != alias);
    if config.skills.sources.len() == before {
        return Err(StackError::SkillSourceNotConfigured { alias });
    }
    // Removing from a valid config yields a valid config, but re-validate for
    // symmetry with the add path so both mutations share one write contract.
    let canonical = config.to_canonical_toml()?;
    crate::config::load_config_from_str(&canonical)?;
    warn_dropped_declarations_healed(&dropped);
    atomic_write_owner_only(&state.runtime_paths.config_path, canonical.as_bytes())?;
    record_skill_event(
        &state,
        "skill.source_remove",
        serde_json::json!({ "alias": &alias }),
    )
    .await;
    Ok(ApiSuccess::new(SkillSourceRemoveResponse {
        alias,
        sources: config.skills.sources.len(),
    }))
}

/// A config write from a leniently loaded view erases any declarations that
/// load dropped. Healing them out is the intended trade, but a hand-edited
/// entry vanishing from the file without a trace is a silent mutation — leave
/// one warning per erased declaration.
fn warn_dropped_declarations_healed(dropped: &crate::config::DroppedDeclarations) {
    for (alias, reason) in &dropped.skill_sources {
        tracing::warn!(
            alias = %alias,
            %reason,
            "config write drops an invalid skill source declaration"
        );
    }
    for (name, reason) in &dropped.mcp_servers {
        tracing::warn!(
            server = %name,
            %reason,
            "config write drops an invalid MCP server declaration"
        );
    }
}

fn agent_install_dir(entry: &RegistryEntry) -> Option<&str> {
    if !entry.supports_agent_skills {
        return None;
    }
    entry.agent_skills_install_dir.as_deref()
}

fn unsupported_skills_agent_error(agent_id: &str) -> StackError {
    StackError::InvalidParam {
        field: "agent",
        reason: format!("agent `{agent_id}` is not a managed Agent Skills install target"),
    }
}
