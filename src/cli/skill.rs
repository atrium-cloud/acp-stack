//! `acps skills` — day-2 Agent Skills management for the active agent.
//!
//! Reads (`list`, `catalog`) go over the session tier — a session key when one
//! is available, otherwise the local UDS socket when keyless access is enabled.
//! Mutations (`add`, `remove`) hit the admin-tier daemon routes. All four are
//! daemon-routed so the hosted platform and the CLI share one code path.

use std::io::{self, IsTerminal};

use clap::{Args, Subcommand};
use serde_json::Value;

use super::core::{
    CliMethod, OutputFormat, SessionAccess, daemon_base_url, daemon_request, encode_path_segment,
    local_daemon_request, print_json, resolve_admin_key, resolve_session_access,
};
use crate::config::{Config, DEFAULT_SKILL_SOURCE_BRANCH};
use crate::error::{Result, StackError};

const LIST_PATH: &str = "/v1/agent/skills";
const CATALOG_PATH: &str = "/v1/agent/skills/catalog";
const ADD_PATH: &str = "/v1/agent/skills/add";
const REMOVE_PATH: &str = "/v1/agent/skills/remove";
const SOURCE_GET_PATH: &str = "/v1/agent/skills/source";
const SOURCE_ADD_PATH: &str = "/v1/agent/skills/sources/add";
const SOURCE_REMOVE_PATH: &str = "/v1/agent/skills/sources/remove";

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// List Agent Skills installed for the active agent.
    List(SkillReadArgs),
    /// Browse the built-in catalog of installable skill sources.
    Catalog(SkillReadArgs),
    /// Install skills from a catalog source, a configured alias, or
    /// `github:<owner>[/<repo>]`.
    #[command(after_help = "Examples:
  acps skills add anthropic docx pptx xlsx pdf
  acps skills add github:my-org my-skill
  acps skills add github:my-org/my-repo my-skill")]
    Add(SkillAddArgs),
    /// Uninstall an installed skill from the active agent.
    Remove(SkillRemoveArgs),
    /// Manage the configured Agent Skills sources.
    #[command(after_help = "Examples:
  acps skills source get anthropic
  acps skills source add my-org my-org/skills --trusted
  acps skills source remove my-org")]
    Source {
        #[command(subcommand)]
        command: SkillSourceCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillSourceCommand {
    /// Inspect a source and list the skills it offers, with metadata.
    Get(SkillSourceGetArgs),
    /// Register a user Agent Skills source in config.
    Add(SkillSourceAddArgs),
    /// Remove a configured user Agent Skills source from config.
    Remove(SkillSourceRemoveArgs),
}

#[derive(Debug, Args)]
pub struct SkillSourceGetArgs {
    /// Catalog alias, configured alias, or `github:<owner>[/<repo>]`.
    source: String,
    /// Session API key. Falls back to ACP_STACK_SESSION_KEY, then the local
    /// socket when keyless session access is enabled.
    #[arg(long = "session-key")]
    session_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SkillSourceAddArgs {
    /// Unique alias for the source.
    alias: String,
    /// GitHub source as `owner/repo`.
    github: String,
    /// Branch to install from.
    #[arg(long, default_value = DEFAULT_SKILL_SOURCE_BRANCH)]
    branch: String,
    /// Assert the source has been vetted (recorded, not enforced).
    #[arg(long)]
    trusted: bool,
    /// Admin API key. Prompted on a TTY when omitted.
    #[arg(long = "admin-key")]
    admin_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SkillSourceRemoveArgs {
    /// Alias of the configured source to remove.
    alias: String,
    /// Admin API key. Prompted on a TTY when omitted.
    #[arg(long = "admin-key")]
    admin_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SkillReadArgs {
    /// Session API key. Falls back to ACP_STACK_SESSION_KEY, then to the local
    /// socket when keyless session access is enabled.
    #[arg(long = "session-key")]
    session_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SkillAddArgs {
    /// Catalog alias (e.g. `anthropic`), a configured alias, or
    /// `github:<owner>[/<repo>]`.
    source: String,
    /// Skill selectors to install (space- or comma-separated).
    #[arg(required = true)]
    skills: Vec<String>,
    /// Admin API key. Prompted on a TTY when omitted.
    #[arg(long = "admin-key")]
    admin_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct SkillRemoveArgs {
    /// Install name of the skill to remove (e.g. `docx` or `zoom/android`).
    /// Only skills installed by acp-stack can be removed; they can always be
    /// re-added with `acps skills add`.
    skill: String,
    /// Admin API key. Prompted on a TTY when omitted.
    #[arg(long = "admin-key")]
    admin_key: Option<String>,
}

pub(super) fn run_skill_command(command: SkillCommand, output: OutputFormat) -> Result<()> {
    // Lenient load: `acps skills source remove` is the repair path for a
    // hand-edited invalid `[[skills.sources]]` entry, so it must not refuse to
    // start because of one.
    let config = Config::load_lenient_from_default_path()?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })?;
    match command {
        SkillCommand::List(args) => {
            let body = runtime.block_on(session_request(
                &config,
                &base_url,
                resolve_session_access(&config, args.session_key)?,
                CliMethod::Get,
                LIST_PATH,
            ))?;
            render(output, &body, print_list)
        }
        SkillCommand::Catalog(args) => {
            let body = runtime.block_on(session_request(
                &config,
                &base_url,
                resolve_session_access(&config, args.session_key)?,
                CliMethod::Get,
                CATALOG_PATH,
            ))?;
            render(output, &body, print_catalog)
        }
        SkillCommand::Add(args) => {
            let interactive = io::stdin().is_terminal();
            let admin_key = resolve_admin_key(args.admin_key.clone(), interactive)?;
            let request = serde_json::json!({
                "source": args.source,
                "skills": args.skills,
            });
            let body = runtime.block_on(daemon_request(
                &base_url,
                CliMethod::Post,
                ADD_PATH,
                &admin_key,
                Some(&request),
            ))?;
            render(output, &body, print_add)
        }
        SkillCommand::Remove(args) => {
            let interactive = io::stdin().is_terminal();
            let admin_key = resolve_admin_key(args.admin_key.clone(), interactive)?;
            let request = serde_json::json!({ "skill": args.skill });
            let body = runtime.block_on(daemon_request(
                &base_url,
                CliMethod::Post,
                REMOVE_PATH,
                &admin_key,
                Some(&request),
            ))?;
            render(output, &body, print_remove)
        }
        SkillCommand::Source { command } => {
            run_skill_source_command(command, output, &config, &base_url, &runtime)
        }
    }
}

fn run_skill_source_command(
    command: SkillSourceCommand,
    output: OutputFormat,
    config: &Config,
    base_url: &str,
    runtime: &tokio::runtime::Runtime,
) -> Result<()> {
    match command {
        SkillSourceCommand::Get(args) => {
            let path = format!(
                "{SOURCE_GET_PATH}?source={}",
                encode_path_segment(&args.source)
            );
            let body = runtime.block_on(session_request(
                config,
                base_url,
                resolve_session_access(config, args.session_key)?,
                CliMethod::Get,
                &path,
            ))?;
            render(output, &body, print_source_get)
        }
        SkillSourceCommand::Add(args) => {
            let admin_key = resolve_admin_key(args.admin_key.clone(), io::stdin().is_terminal())?;
            let request = serde_json::json!({
                "alias": args.alias,
                "github": args.github,
                "branch": args.branch,
                "trusted": args.trusted,
            });
            let body = runtime.block_on(daemon_request(
                base_url,
                CliMethod::Post,
                SOURCE_ADD_PATH,
                &admin_key,
                Some(&request),
            ))?;
            render(output, &body, print_source_add)
        }
        SkillSourceCommand::Remove(args) => {
            let admin_key = resolve_admin_key(args.admin_key.clone(), io::stdin().is_terminal())?;
            let request = serde_json::json!({ "alias": args.alias });
            let body = runtime.block_on(daemon_request(
                base_url,
                CliMethod::Post,
                SOURCE_REMOVE_PATH,
                &admin_key,
                Some(&request),
            ))?;
            render(output, &body, print_source_remove)
        }
    }
}

async fn session_request(
    config: &Config,
    base_url: &str,
    access: SessionAccess,
    method: CliMethod,
    path: &str,
) -> Result<Value> {
    match access {
        SessionAccess::Bearer(session_key) => {
            daemon_request(base_url, method, path, &session_key, None).await
        }
        SessionAccess::Local => local_daemon_request(config, method, path, None).await,
    }
}

fn render(output: OutputFormat, body: &Value, print_text: fn(&Value)) -> Result<()> {
    let data = body.get("data").unwrap_or(body);
    if output.is_json() {
        return print_json(data);
    }
    print_text(data);
    Ok(())
}

fn print_list(data: &Value) {
    let agent = data.get("agent_id").and_then(Value::as_str).unwrap_or("");
    println!("agent: {agent}");
    if !data
        .get("supported")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        println!("skills: agent `{agent}` is not a managed Agent Skills target");
        return;
    }
    let skills = data
        .get("skills")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if skills.is_empty() {
        println!("skills: none installed");
        return;
    }
    println!("skills: {} installed", skills.len());
    for skill in &skills {
        if let Some(name) = skill.get("name").and_then(Value::as_str) {
            match skill.get("source").and_then(Value::as_str) {
                Some(source) => println!("  {name} ({source})"),
                None => println!("  {name} (unmanaged)"),
            }
        }
    }
}

fn print_catalog(data: &Value) {
    let sources = data
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    println!("skill sources: {}", sources.len());
    for source in &sources {
        let alias = source.get("alias").and_then(Value::as_str).unwrap_or("");
        let id = source.get("id").and_then(Value::as_str).unwrap_or("");
        let repo = source.get("repo").and_then(Value::as_str).unwrap_or("");
        println!("{alias} ({id}) — {repo}");
        let skills = join_str_array(source.get("skills"));
        if skills.is_empty() {
            println!("  skills: (none indexed)");
        } else {
            println!("  skills: {skills}");
        }
        let essential = join_str_array(source.get("essential"));
        if !essential.is_empty() {
            println!("  essential: {essential}");
        }
    }
}

fn print_add(data: &Value) {
    let install = data.get("install").cloned().unwrap_or(Value::Null);
    let source_id = install
        .get("source_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let destination = install
        .get("destination_root")
        .and_then(Value::as_str)
        .unwrap_or("");
    println!("skills add: source {source_id} -> {destination}");
    print_named_group(&install, "installed", "installed");
    print_named_group(&install, "skipped", "already installed");
    print_skills_link(data);
}

fn print_remove(data: &Value) {
    let remove = data.get("remove").cloned().unwrap_or(Value::Null);
    let name = remove
        .get("removed")
        .and_then(|removed| removed.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let root = remove
        .get("install_root")
        .and_then(Value::as_str)
        .unwrap_or("");
    println!("skills remove: uninstalled `{name}` from {root}");
    print_skills_link(data);
}

fn print_named_group(install: &Value, field: &str, label: &str) {
    let entries = install
        .get(field)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        return;
    }
    println!("{label}: {}", entries.len());
    for entry in &entries {
        if let Some(name) = entry.get("name").and_then(Value::as_str) {
            println!("  {name}");
        }
    }
}

fn print_skills_link(data: &Value) {
    if let Some(link) = data.get("skills_link") {
        let link_root = link.get("link_root").and_then(Value::as_str).unwrap_or("");
        let count = |field: &str| {
            link.get(field)
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        let mut parts = Vec::new();
        for (field, label) in [
            ("linked", "linked"),
            ("unchanged", "unchanged"),
            ("conflicts", "kept existing"),
            ("pruned", "pruned dangling"),
        ] {
            let value = count(field);
            if value > 0 {
                parts.push(format!("{label} {value}"));
            }
        }
        if !parts.is_empty() {
            println!("skills link: {} -> {link_root}", parts.join(", "));
        }
        if let Some(errors) = link.get("errors").and_then(Value::as_array) {
            for error in errors.iter().filter_map(Value::as_str) {
                println!("warning: skill link failed: {error}");
            }
        }
    }
    if let Some(error) = data.get("skills_link_error").and_then(Value::as_str) {
        println!("warning: skill link refresh failed: {error}");
    }
}

fn print_source_get(data: &Value) {
    let id = data.get("id").and_then(Value::as_str).unwrap_or("");
    let repo = data.get("repo").and_then(Value::as_str).unwrap_or("");
    let branch = data.get("branch").and_then(Value::as_str).unwrap_or("");
    let kind = if data
        .get("catalog")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "catalog"
    } else {
        "user"
    };
    let trusted = data
        .get("trusted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("source: {id} ({kind}, trusted: {trusted})");
    println!("repo: {repo}@{branch}");
    let skills = data
        .get("skills")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if skills.is_empty() {
        println!("skills: none found");
        return;
    }
    println!("skills: {}", skills.len());
    for skill in &skills {
        let selector = skill.get("selector").and_then(Value::as_str).unwrap_or("");
        match skill.get("description").and_then(Value::as_str) {
            Some(description) if !description.is_empty() => {
                println!("  {selector} — {description}")
            }
            _ => println!("  {selector}"),
        }
    }
}

fn print_source_add(data: &Value) {
    let alias = data.get("alias").and_then(Value::as_str).unwrap_or("");
    let github = data.get("github").and_then(Value::as_str).unwrap_or("");
    let branch = data.get("branch").and_then(Value::as_str).unwrap_or("");
    let trusted = data
        .get("trusted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("skill source added: {alias} -> {github}@{branch} (trusted: {trusted})");
    if let Some(count) = data.get("sources").and_then(Value::as_u64) {
        println!("configured sources: {count}");
    }
}

fn print_source_remove(data: &Value) {
    let alias = data.get("alias").and_then(Value::as_str).unwrap_or("");
    println!("skill source removed: {alias}");
    if let Some(count) = data.get("sources").and_then(Value::as_u64) {
        println!("configured sources: {count}");
    }
}

fn join_str_array(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}
