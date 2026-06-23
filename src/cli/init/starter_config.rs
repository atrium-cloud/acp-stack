use std::io::IsTerminal;
use std::path::Path;

use http::header::HeaderName;

use crate::config::{
    self, AgentConfig, AgentInstallConfig, ApiConfig, CodeSourceConfig, Config, DataSourceConfig,
    DependencyEntry, DependencyInstallAction, DependencyInstallScope, EdgeConfig, HttpHeaderRef,
    LoggingConfig, McpConfig, McpHttpServer, McpServerConfig, McpStdioServer, SecurityConfig,
    SecurityHttpConfig, StackUpdatePolicy, SupabaseLoggingConfig, WorkspaceConfig,
    is_valid_secret_ref_name, normalize_day_or_week_duration,
};
use crate::error::{Result, StackError};
use crate::runtime::dependencies::deps_apply::{
    DepApplyCandidate, candidate_summary_line, summarize_candidates,
};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::{
    ANTHROPIC_SKILLS_SOURCE_ID, OPENAI_PLUGINS_SOURCE_ID, SOURCE_ANTHROPIC, SOURCE_OPENAI,
};
use crate::runtime::install::skill_registry::SkillCatalog;
use crate::secrets::SecretStore;

use super::super::logging::{
    SUPABASE_DEFAULT_API_KEY_REF, SUPABASE_DEFAULT_SCHEMA, disabled_supabase_config,
    enabled_supabase_config,
};
use super::{
    InitArgs, InitMcpHttpHeader, InitMcpHttpServer, InitMcpStdioServer, STARTER_AGENT_COMMAND,
    STARTER_AGENT_ID, STARTER_AGENT_INSTALL_COMMAND, STARTER_AGENT_INSTALL_CREATES,
    STARTER_AGENT_INSTALL_TYPE, STARTER_AGENT_NAME, STARTER_AGENT_RESTART,
    STARTER_AUTH_BLOCK_DURATION, STARTER_AUTH_FAILURES_PER_MINUTE, STARTER_DEFAULT_SHELL,
    STARTER_LOCAL_RETENTION_DAYS, STARTER_LOG_LEVEL, STARTER_MAX_REQUEST_BYTES,
    STARTER_RATE_LIMIT_BURST, STARTER_RATE_LIMIT_PER_MINUTE, STARTER_WORKSPACE_MAX_FILE_BYTES,
    prompt, prompts_enabled,
};

const STANDARD_AGENT_WORK_FEATURE: &str = "agent-work";
const STANDARD_AGENT_WORK_BUNDLE_NAME: &str = "acp-stack-agent-work-base";
const STANDARD_AGENT_WORK_BUNDLE_CREATES: &str = "/usr/local/share/acp-stack/agent-work-base.done";
const BROWSER_USE_FEATURE: &str = "browser";
const BROWSER_USE_MCP_COMMAND: &str = "acp-stack-browser-use-mcp";
const AGENT_WORK_PYTHON_VERSION: &str = "3.14";
const AGENT_WORK_PYTHON_INSTALL_DIR: &str = "/opt/acp-stack/python";
const BROWSER_USE_PYTHON_VERSION: &str = "3.14";
const BROWSER_USE_PREFIX: &str = "/opt/acp-stack/browser-use";
const BROWSER_USE_SHARE_DIR: &str = "/usr/local/share/acp-stack";
const BROWSER_USE_WRAPPER_PATH: &str = "/usr/local/share/acp-stack/browser-use-mcp.py";
const BROWSER_USE_LAUNCHER_PATH: &str = "/usr/local/bin/acp-stack-browser-use-mcp";

// Centralized package manifest for init's Standard Setup path. This mirrors the
// VM base profile: broad agent-work tools, no build toolchains or language
// headers, and no inferred package-manager behavior.
const STANDARD_AGENT_WORK_APT_PACKAGES: &[&str] = &[
    "ca-certificates",
    "bash",
    "curl",
    "git",
    "openssh-client",
    "nodejs",
    "npm",
    "python3",
    "python3-venv",
    "tar",
    "gzip",
    "xz-utils",
    "zstd",
    "unzip",
    "zip",
    "jq",
    "ripgrep",
    "patch",
    "diffutils",
    "procps",
];

const STANDARD_AGENT_WORK_COMMANDS: &[&str] = &[
    "bash",
    "curl",
    "git",
    "ssh",
    "node",
    "npm",
    "python3",
    "python3.14",
    "uv",
    "tar",
    "gzip",
    "xz",
    "zstd",
    "unzip",
    "zip",
    "jq",
    "rg",
    "patch",
    "diff",
    "ps",
];

const BROWSER_USE_APT_PACKAGES: &[&str] = &[
    "ca-certificates",
    "curl",
    "fonts-noto",
    "fonts-noto-color-emoji",
    "fonts-noto-cjk",
    "fonts-liberation",
    "fonts-dejavu",
    "fonts-freefont-ttf",
];

const BUILD_HEAVY_APT_PACKAGES: &[&str] = &["build-essential", "pkg-config", "python3-dev"];

pub(super) fn validate_deployment_overrides_match_existing(
    args: &InitArgs,
    config: &Config,
) -> Result<()> {
    reject_conflicting_deployment_override(
        "--workspace-root",
        args.workspace_root.as_deref(),
        &config.workspace.root,
    )?;
    reject_conflicting_deployment_override(
        "--workspace-uploads",
        args.workspace_uploads.as_deref(),
        &config.workspace.uploads,
    )?;
    reject_conflicting_deployment_override(
        "--runtime-user",
        args.runtime_user.as_deref(),
        &config.workspace.runtime_user,
    )
}

pub(super) fn reject_starter_only_mcp_args_for_existing_config(args: &InitArgs) -> Result<()> {
    reject_starter_only_mcp_arg("--mcp-preset", &args.mcp_preset)?;
    reject_starter_only_mcp_arg("--mcp-stdio", &args.mcp_stdio)?;
    reject_starter_only_mcp_arg("--mcp-stdio-env", &args.mcp_stdio_env)?;
    reject_starter_only_mcp_arg("--mcp-http", &args.mcp_http)?;
    reject_starter_only_mcp_arg("--mcp-http-header", &args.mcp_http_header)
}

/// Operator-supplied agent environment variable references collected during
/// init. `flag_refs` (from `--agent-env-ref`) must already exist in the secret
/// store; `fresh` holds interactively-entered name+value pairs to write.
/// Values are `Zeroizing` and never echoed or recorded in the init run args.
#[derive(Default)]
pub(super) struct AgentEnvCollection {
    flag_refs: Vec<String>,
    fresh: Vec<(String, zeroize::Zeroizing<String>)>,
}

impl AgentEnvCollection {
    /// All declared ref names, flag-provided first then interactive.
    fn ref_names(&self) -> Vec<String> {
        self.flag_refs
            .iter()
            .cloned()
            .chain(self.fresh.iter().map(|(name, _)| name.clone()))
            .collect()
    }
}

/// Reject `--agent-env-ref` when a config already exists; like the other
/// starter-only flags it only applies to a fresh config.
pub(super) fn reject_agent_env_refs_for_existing_config(args: &InitArgs) -> Result<()> {
    if !args.agent_env_ref.is_empty() {
        return Err(StackError::InvalidParam {
            field: "--agent-env-ref",
            reason: "agent env refs can only be set while creating a new config".to_owned(),
        });
    }
    Ok(())
}

/// Collect operator agent environment variable refs from `--agent-env-ref` and,
/// in interactive runs, name/value entries. Flag refs reference secrets that
/// must already exist; interactive entries carry their value for the store write
/// after the secret store opens.
pub(super) fn collect_agent_env_refs_for_init(
    args: &InitArgs,
    interactive: bool,
) -> Result<AgentEnvCollection> {
    let mut flag_refs: Vec<String> = Vec::new();
    for raw in &args.agent_env_ref {
        let name = raw.trim().to_owned();
        if name.is_empty() {
            return Err(StackError::InvalidParam {
                field: "agent-env-ref",
                reason: "secret ref name must not be empty".to_owned(),
            });
        }
        if !is_valid_secret_ref_name(&name) {
            return Err(StackError::InvalidParam {
                field: "agent-env-ref",
                reason: format!(
                    "`{name}` is not a valid secret ref name (letters, digits, and underscore; must not start with a digit)"
                ),
            });
        }
        if !flag_refs.contains(&name) {
            flag_refs.push(name);
        }
    }
    let mut fresh: Vec<(String, zeroize::Zeroizing<String>)> = Vec::new();
    if interactive && args.prompt_agent_env_refs {
        loop {
            let Some(name) = prompt::text(interactive, "secret ref name (blank to finish)", false)?
            else {
                break;
            };
            let name = name.trim().to_owned();
            if name.is_empty() {
                break;
            }
            if !is_valid_secret_ref_name(&name) {
                println!(
                    "`{name}` is not a valid secret ref name (letters, digits, and underscore; must not start with a digit); skipping."
                );
                continue;
            }
            let Some(value) = prompt::password(interactive, &format!("value for {name}"))? else {
                break;
            };
            if value.is_empty() {
                // Don't store an empty secret for the ref; skip it.
                continue;
            }
            fresh.push((name, zeroize::Zeroizing::new(value)));
        }
    }
    Ok(AgentEnvCollection { flag_refs, fresh })
}

/// Append the collected ref names to `config.agent.env`, de-duplicating against
/// refs already present (e.g. the provider key ref). Returns whether anything
/// was added. Called only after the refs are verified/stored so a run that fails
/// verification never persists an unresolved `agent.env` ref.
pub(super) fn append_agent_env_refs(config: &mut Config, collection: &AgentEnvCollection) -> bool {
    let mut changed = false;
    for name in collection.ref_names() {
        if !config.agent.env.contains(&name) {
            config.agent.env.push(name);
            changed = true;
        }
    }
    changed
}

/// Write interactively-collected env values to the store and verify that every
/// flag-provided ref already resolves. Runs after the secret store is open and
/// before the agent is installed/launched, so `resolve_agent_env` finds them.
pub(super) fn apply_agent_env_collection(
    secret_store: &mut SecretStore,
    collection: &AgentEnvCollection,
) -> Result<()> {
    // Guard the store before writing. `set_many` upserts, so a fresh name that
    // collides would silently overwrite an existing provider/MCP secret.
    for (name, _) in &collection.fresh {
        if !is_valid_secret_ref_name(name) {
            return Err(StackError::InvalidParam {
                field: "agent-env-ref",
                reason: format!(
                    "`{name}` is not a valid secret ref name (letters, digits, and underscore; must not start with a digit)"
                ),
            });
        }
        if secret_store.contains(name) {
            return Err(StackError::InvalidParam {
                field: "agent-env-ref",
                reason: format!(
                    "secret `{name}` already exists in the store; refusing to overwrite it. Choose a new ref name, or update the value with `acps secrets set`."
                ),
            });
        }
    }
    // Only write when there is something to store: `set_many` re-encrypts the
    // whole store (age ciphertext is non-deterministic), so an empty write on a
    // no-change re-run would needlessly rewrite the secret file.
    if !collection.fresh.is_empty() {
        secret_store.set_many(
            collection
                .fresh
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )?;
    }
    for name in &collection.flag_refs {
        if !secret_store.contains(name) {
            return Err(StackError::SecretNotFound { name: name.clone() });
        }
    }
    Ok(())
}

fn parse_dep_entry(
    raw: &str,
    scope: DependencyInstallScope,
    flag: &'static str,
) -> Result<DependencyEntry> {
    let Some((name, shell)) = raw.split_once('=') else {
        return Err(StackError::InvalidParam {
            field: flag,
            reason: format!("expected NAME=SHELL, got `{raw}`"),
        });
    };
    let name = name.trim().to_owned();
    let shell = shell.trim().to_owned();
    if name.is_empty() || shell.is_empty() {
        return Err(StackError::InvalidParam {
            field: flag,
            reason: format!("both a name and a shell command are required in `{raw}`"),
        });
    }
    Ok(DependencyEntry {
        name,
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell,
            creates: None,
            scope,
            timeout_secs: None,
        }),
    })
}

/// Build dependency entries from `--dep` (user scope) and `--dep-system`
/// (system scope) flags. Each is `NAME=SHELL` with an install action.
pub(super) fn deps_from_args(args: &InitArgs) -> Result<Vec<DependencyEntry>> {
    let mut entries = Vec::new();
    for raw in &args.dep {
        entries.push(parse_dep_entry(raw, DependencyInstallScope::User, "--dep")?);
    }
    for raw in &args.dep_system {
        entries.push(parse_dep_entry(
            raw,
            DependencyInstallScope::System,
            "--dep-system",
        )?);
    }
    Ok(entries)
}

fn standard_agent_work_install_shell() -> String {
    let packages = STANDARD_AGENT_WORK_APT_PACKAGES.join(" ");
    format!(
        r#"set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends {packages}
if ! command -v uv >/dev/null 2>&1; then
  tmp_installer="$(mktemp)"
  trap 'rm -f "${{tmp_installer}}"' EXIT
  curl -LsSf https://astral.sh/uv/install.sh -o "${{tmp_installer}}"
  UV_INSTALL_DIR=/usr/local/bin UV_NO_MODIFY_PATH=1 sh "${{tmp_installer}}"
fi
install -d -m 0755 /usr/local/bin
if ! command -v python3.14 >/dev/null 2>&1; then
  UV_PYTHON_INSTALL_DIR={python_install_dir} UV_PYTHON_BIN_DIR=/usr/local/bin uv python install {python_version}
fi
command -v python3.14 >/dev/null 2>&1
install -d -m 0755 /usr/local/share/acp-stack
: > {bundle_marker}
chmod 0755 {bundle_marker}"#,
        python_install_dir = AGENT_WORK_PYTHON_INSTALL_DIR,
        python_version = AGENT_WORK_PYTHON_VERSION,
        bundle_marker = STANDARD_AGENT_WORK_BUNDLE_CREATES,
    )
}

fn browser_use_launcher_script() -> String {
    include_str!("../../../scripts/browser-use-mcp")
        .replace("@BROWSER_USE_VENV@", BROWSER_USE_PREFIX)
        .replace("@BROWSER_USE_MCP_SCRIPT@", BROWSER_USE_WRAPPER_PATH)
}

fn browser_use_install_shell() -> String {
    let packages = BROWSER_USE_APT_PACKAGES.join(" ");
    let launcher_script = browser_use_launcher_script();
    let wrapper_script = include_str!("../../../scripts/browser-use-mcp.py");
    format!(
        r#"set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends {packages}
if ! command -v uv >/dev/null 2>&1; then
  tmp_installer="$(mktemp)"
  trap 'rm -f "${{tmp_installer}}"' EXIT
  curl -LsSf https://astral.sh/uv/install.sh -o "${{tmp_installer}}"
  UV_INSTALL_DIR=/usr/local/bin UV_NO_MODIFY_PATH=1 sh "${{tmp_installer}}"
fi
if apt-cache show chromium >/dev/null 2>&1; then
  chromium_package=chromium
elif apt-cache show chromium-browser >/dev/null 2>&1; then
  chromium_package=chromium-browser
else
  echo "no Chromium package found in apt metadata" >&2
  exit 1
fi
apt-get install -y -qq --no-install-recommends "${{chromium_package}}"
install -d -m 0755 "$(dirname "{browser_prefix}")" "{browser_share_dir}" "$(dirname "{browser_launcher}")"
uv venv --python {python_version} "{browser_prefix}"
"{browser_prefix}/bin/python" - <<'PY'
import sys

if sys.version_info < (3, 11):
    raise SystemExit(f"Browser Use requires Python 3.11+; venv has {{sys.version.split()[0]}}")
PY
uv pip install --python "{browser_prefix}/bin/python" --upgrade 'browser-use[core]'
"{browser_prefix}/bin/browser-use" install
cat > "{browser_wrapper}" <<'ACP_STACK_BROWSER_USE_MCP_PY'
{wrapper_script}
ACP_STACK_BROWSER_USE_MCP_PY
chmod 0644 "{browser_wrapper}"
cat > "{browser_launcher}" <<'ACP_STACK_BROWSER_USE_MCP_SH'
{launcher_script}
ACP_STACK_BROWSER_USE_MCP_SH
chmod 0755 "{browser_launcher}"
command -v {browser_command} >/dev/null 2>&1
"{browser_launcher}" --help >/dev/null"#,
        browser_command = BROWSER_USE_MCP_COMMAND,
        browser_launcher = BROWSER_USE_LAUNCHER_PATH,
        browser_prefix = BROWSER_USE_PREFIX,
        browser_share_dir = BROWSER_USE_SHARE_DIR,
        browser_wrapper = BROWSER_USE_WRAPPER_PATH,
        python_version = BROWSER_USE_PYTHON_VERSION,
        wrapper_script = wrapper_script.trim_end(),
        launcher_script = launcher_script.trim_end(),
    )
}

fn check_only_dependency(name: &str, feature: &str) -> DependencyEntry {
    DependencyEntry {
        name: name.to_owned(),
        required: true,
        feature: Some(feature.to_owned()),
        install: None,
    }
}

fn push_unique_dependency(
    category: &'static str,
    dependencies: &mut Vec<DependencyEntry>,
    entry: DependencyEntry,
) -> Result<()> {
    if dependencies
        .iter()
        .any(|existing| existing.name == entry.name)
    {
        return Err(StackError::InvalidParam {
            field: "dependencies",
            reason: format!(
                "dependency `{}` is already declared under {category}",
                entry.name
            ),
        });
    }
    dependencies.push(entry);
    Ok(())
}

fn push_standard_agent_work_deps_to_config(config: &mut Config) -> Result<()> {
    push_unique_dependency(
        "commands",
        &mut config.dependencies.commands,
        DependencyEntry {
            name: STANDARD_AGENT_WORK_BUNDLE_NAME.to_owned(),
            required: true,
            feature: Some(STANDARD_AGENT_WORK_FEATURE.to_owned()),
            install: Some(DependencyInstallAction {
                shell: standard_agent_work_install_shell(),
                creates: Some(STANDARD_AGENT_WORK_BUNDLE_CREATES.to_owned()),
                scope: DependencyInstallScope::System,
                timeout_secs: None,
            }),
        },
    )?;
    for command in STANDARD_AGENT_WORK_COMMANDS {
        push_unique_dependency(
            "commands",
            &mut config.dependencies.commands,
            check_only_dependency(command, STANDARD_AGENT_WORK_FEATURE),
        )?;
    }
    for package in STANDARD_AGENT_WORK_APT_PACKAGES {
        push_unique_dependency(
            "packages",
            &mut config.dependencies.packages,
            check_only_dependency(package, STANDARD_AGENT_WORK_FEATURE),
        )?;
    }
    assert_standard_agent_work_excludes_build_packages()?;
    Ok(())
}

fn push_browser_use_profile_to_config(config: &mut Config) -> Result<()> {
    push_unique_dependency(
        "commands",
        &mut config.dependencies.commands,
        DependencyEntry {
            name: BROWSER_USE_MCP_COMMAND.to_owned(),
            required: true,
            feature: Some(BROWSER_USE_FEATURE.to_owned()),
            install: Some(DependencyInstallAction {
                shell: browser_use_install_shell(),
                creates: Some(BROWSER_USE_MCP_COMMAND.to_owned()),
                scope: DependencyInstallScope::System,
                timeout_secs: None,
            }),
        },
    )
}

fn assert_standard_agent_work_excludes_build_packages() -> Result<()> {
    for package in STANDARD_AGENT_WORK_APT_PACKAGES {
        if BUILD_HEAVY_APT_PACKAGES.contains(package) {
            return Err(StackError::InvalidParam {
                field: "standard setup",
                reason: format!("standard dependency profile must not include `{package}`"),
            });
        }
    }
    Ok(())
}

/// Append flag-declared dependencies to `config.dependencies.commands`,
/// rejecting a name that is already declared (e.g. an auto-added `cloudflared`).
pub(super) fn push_args_deps_to_config(config: &mut Config, args: &InitArgs) -> Result<()> {
    if args.standard_agent_work_deps {
        push_standard_agent_work_deps_to_config(config)?;
    }
    if args.browser_use_profile {
        push_browser_use_profile_to_config(config)?;
    }
    for entry in deps_from_args(args)? {
        if config
            .dependencies
            .commands
            .iter()
            .any(|existing| existing.name == entry.name)
        {
            return Err(StackError::InvalidParam {
                field: "--dep",
                reason: format!("dependency `{}` is already declared", entry.name),
            });
        }
        config.dependencies.commands.push(entry);
    }
    Ok(())
}

/// `--dep`/`--dep-system` declare into a fresh starter config only; reject them
/// when a config already exists (the operator edits config or uses `acps deps`).
pub(super) fn reject_deps_args_for_existing_config(args: &InitArgs) -> Result<()> {
    for (flag, values) in [("--dep", &args.dep), ("--dep-system", &args.dep_system)] {
        if !values.is_empty() {
            return Err(StackError::InvalidParam {
                field: flag,
                reason: "dependency declarations apply only when creating a starter config"
                    .to_owned(),
            });
        }
    }
    Ok(())
}

/// Decide whether to run the dependency-apply init step. Non-interactive runs
/// require `--deps-apply --deps-apply-yes`; interactive runs summarize the
/// pending actions and confirm (default no). Returns false when there is
/// nothing actionable.
pub(super) fn should_apply_deps_for_init(
    args: &InitArgs,
    candidates: &[DepApplyCandidate],
    interactive: bool,
) -> Result<bool> {
    if candidates.is_empty() {
        return Ok(false);
    }
    if !interactive {
        if args.deps_apply && !args.deps_apply_yes {
            return Err(StackError::InvalidParam {
                field: "--deps-apply",
                reason: "non-interactive dependency apply requires --deps-apply-yes".to_owned(),
            });
        }
        return Ok(args.deps_apply && args.deps_apply_yes);
    }
    if args.deps_apply && args.deps_apply_yes {
        return Ok(true);
    }
    let (count, any_system) = summarize_candidates(candidates);
    println!("dependencies with install actions ({count}):");
    for candidate in candidates {
        println!("  - {}", candidate_summary_line(candidate));
    }
    if any_system {
        println!("note: one or more actions declare scope=system and require root privilege.");
    }
    prompt::confirm(interactive, "Apply these dependencies now?", false)
}

/// Validate an acp-stack auto-update frequency. The minimum granularity is a
/// day, so only `d` (day) and `w` (week) units are accepted — the shared
/// duration parser treats `m` as minutes, so allowing it here would silently
/// schedule sub-day updates. For longer cadences use weeks (e.g. `26w` ≈ 6
/// months).
fn validate_update_frequency(raw: &str) -> Result<String> {
    normalize_day_or_week_duration("stack-update-frequency", raw)
}

fn parse_stack_update_policy(raw: &str) -> Result<StackUpdatePolicy> {
    match raw {
        "on" => Ok(StackUpdatePolicy::Compatible),
        "security" => Ok(StackUpdatePolicy::SecurityCritical),
        "off" => Ok(StackUpdatePolicy::Manual),
        other => Err(StackError::InvalidParam {
            field: "--stack-update",
            reason: format!("expected on|security|off, got `{other}`"),
        }),
    }
}

pub(super) fn validate_stack_update_args(args: &InitArgs) -> Result<()> {
    let policy = args
        .stack_update
        .as_deref()
        .map(parse_stack_update_policy)
        .transpose()?;
    if policy != Some(StackUpdatePolicy::Manual)
        && let Some(raw) = args.stack_update_frequency.as_deref()
    {
        validate_update_frequency(raw)?;
    }
    Ok(())
}

fn prompt_stack_update_policy() -> Result<StackUpdatePolicy> {
    let items = vec![
        (
            StackUpdatePolicy::SecurityCritical,
            "Security updates only".to_owned(),
            "recommended".to_owned(),
        ),
        (
            StackUpdatePolicy::Compatible,
            "On — all compatible updates".to_owned(),
            String::new(),
        ),
        (
            StackUpdatePolicy::Manual,
            "Off — manual updates only".to_owned(),
            String::new(),
        ),
    ];
    Ok(prompt::select(true, "acp-stack auto-update", &items)?
        .unwrap_or(StackUpdatePolicy::SecurityCritical))
}

fn prompt_stack_update_frequency() -> Result<String> {
    #[derive(Clone, PartialEq, Eq)]
    enum FrequencyChoice {
        Daily,
        Weekly,
        Custom,
    }
    let items = vec![
        (
            FrequencyChoice::Daily,
            "Daily (1d)".to_owned(),
            String::new(),
        ),
        (
            FrequencyChoice::Weekly,
            "Weekly (1w)".to_owned(),
            String::new(),
        ),
        (
            FrequencyChoice::Custom,
            "Custom".to_owned(),
            "day/week units, e.g. 3w".to_owned(),
        ),
    ];
    match prompt::select(true, "update frequency", &items)? {
        Some(FrequencyChoice::Weekly) => Ok("1w".to_owned()),
        Some(FrequencyChoice::Custom) => {
            let raw = prompt::text(true, "frequency (e.g. 3w; minimum 1 day)", true)?
                .unwrap_or_else(|| "1d".to_owned());
            validate_update_frequency(&raw)
        }
        // Daily, or a non-interactive/empty select, defaults to daily.
        _ => Ok("1d".to_owned()),
    }
}

/// Configure `[updates.acp_stack]` from `--stack-update`/`--stack-update-frequency`
/// or, interactively, a policy + frequency prompt placed after model selection.
/// `on` → Compatible, `security` → SecurityCritical, `off` → Manual. A frequency
/// is only collected for non-Manual policies. Returns whether config changed; a
/// non-interactive run with no flags leaves the schema defaults intact.
pub(super) fn configure_stack_update_for_init(
    args: &InitArgs,
    config: &mut Config,
    interactive: bool,
) -> Result<bool> {
    let policy = match args.stack_update.as_deref() {
        Some(raw) => Some(parse_stack_update_policy(raw)?),
        None if interactive => Some(prompt_stack_update_policy()?),
        None => None,
    };
    let Some(policy) = policy else {
        return Ok(false);
    };
    let frequency = if policy == StackUpdatePolicy::Manual {
        None
    } else {
        match args.stack_update_frequency.as_deref() {
            Some(raw) => Some(validate_update_frequency(raw)?),
            None if interactive => Some(prompt_stack_update_frequency()?),
            None => None,
        }
    };

    let mut changed = false;
    if config.updates.acp_stack.policy != policy {
        config.updates.acp_stack.policy = policy;
        changed = true;
    }
    if let Some(frequency) = frequency
        && config.updates.acp_stack.frequency != frequency
    {
        config.updates.acp_stack.frequency = frequency;
        changed = true;
    }
    Ok(changed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvironmentSetupPath {
    Standard,
    Advanced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpTransportChoice {
    Stdio,
    Http,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupRowAction {
    AddAnother,
    Discard,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DataSourceKind {
    Local,
    Https,
    S3,
}

pub(super) fn prompt_environment_configuration_if_needed(
    args: &mut InitArgs,
    registry: &RegistryCatalog,
    skill_catalog: &SkillCatalog,
) -> Result<()> {
    let interactive = prompts_enabled(args);
    if !interactive {
        return Ok(());
    }
    let setup_path = prompt::select(
        interactive,
        "Environment configuration",
        &[
            (
                EnvironmentSetupPath::Standard,
                "Standard Setup".to_owned(),
                "Opinionated defaults: essential dependencies, browser-use, skills, data sources"
                    .to_owned(),
            ),
            (
                EnvironmentSetupPath::Advanced,
                "Advanced Setup".to_owned(),
                "Clean slate: custom dependencies, skills, MCP, agent env, data sources".to_owned(),
            ),
        ],
    )?;
    // An interactive terminal always yields a choice (Esc aborts init upstream),
    // so `None` only arises when a hosted driver leaves this out-of-v1-scope
    // prompt unhandled; that skips environment configuration like a
    // non-interactive run rather than failing.
    match setup_path {
        Some(EnvironmentSetupPath::Standard) => {
            prompt_standard_setup(interactive, args, registry, skill_catalog)
        }
        Some(EnvironmentSetupPath::Advanced) => prompt_advanced_setup(interactive, args, registry),
        None => Ok(()),
    }
}

// Standard Setup: up to four opinionated prompts. Declining every offered one is
// the intended "set it up later" path, so there is deliberately no separate
// skip option.
fn prompt_standard_setup(
    interactive: bool,
    args: &mut InitArgs,
    registry: &RegistryCatalog,
    skill_catalog: &SkillCatalog,
) -> Result<()> {
    if prompt::confirm(interactive, "Install essential dependencies?", true)? {
        args.standard_agent_work_deps = true;
    }
    if prompt::confirm(interactive, "Install browser-use?", false)? {
        args.browser_use_profile = true;
    }
    if agent_supports_skills(args, registry)
        && essential_agent_skills_available(skill_catalog)
        && !args.no_skills
        && args.skills_source.is_none()
        && args.skills.is_empty()
        && args.plugins_source.is_none()
        && args.plugins.is_empty()
        && prompt::confirm(interactive, "Add essential agent skills?", false)?
    {
        apply_essential_agent_skills(args, skill_catalog);
    }
    if args.data_from.is_empty()
        && args.prompt_data_sources.is_empty()
        && prompt::confirm(interactive, "Add data sources now?", false)?
    {
        prompt_data_sources(interactive, args)?;
    }
    Ok(())
}

// Advanced Setup: a clean slate of up to five opt-in prompts. Each is gated on
// the matching values not already arriving by flag, so flags suppress re-prompts.
// MCP lives only here on purpose: editing servers through a shell wizard is
// awkward, and operators can add them later by editing config.
fn prompt_advanced_setup(
    interactive: bool,
    args: &mut InitArgs,
    registry: &RegistryCatalog,
) -> Result<()> {
    if args.dep.is_empty()
        && args.dep_system.is_empty()
        && prompt::confirm(interactive, "Add custom dependencies?", false)?
    {
        prompt_deps(interactive, args)?;
    }
    if agent_supports_skills(args, registry)
        && !args.no_skills
        && args.skills_source.is_none()
        && args.skills.is_empty()
        && args.plugins_source.is_none()
        && args.plugins.is_empty()
        && prompt::confirm(interactive, "Add agent skills?", false)?
    {
        args.prompt_skills = true;
    }
    if args.mcp_stdio.is_empty()
        && args.prompt_mcp_stdio.is_empty()
        && args.mcp_http.is_empty()
        && args.prompt_mcp_http.is_empty()
        && prompt::confirm(interactive, "Add MCP servers?", false)?
    {
        prompt_mcp_servers(interactive, args)?;
    }
    if args.agent_env_ref.is_empty()
        && prompt::confirm(interactive, "Add agent environment variables?", false)?
    {
        args.prompt_agent_env_refs = true;
    }
    if args.data_from.is_empty()
        && args.prompt_data_sources.is_empty()
        && prompt::confirm(interactive, "Add data sources now?", false)?
    {
        prompt_data_sources(interactive, args)?;
    }
    Ok(())
}

// "Add MCP servers" is one Advanced step spanning both transports: the operator
// picks a transport, adds rows for it, and repeats until choosing Done.
fn prompt_mcp_servers(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let choice = prompt::select(
            interactive,
            "MCP transport",
            &[
                (
                    McpTransportChoice::Stdio,
                    "stdio server".to_owned(),
                    "Local command, args, env refs".to_owned(),
                ),
                (
                    McpTransportChoice::Http,
                    "HTTP server".to_owned(),
                    "Remote URL and header refs".to_owned(),
                ),
                (
                    McpTransportChoice::Done,
                    "Done".to_owned(),
                    "Finish adding MCP servers".to_owned(),
                ),
            ],
        )?;
        match choice {
            Some(McpTransportChoice::Stdio) => prompt_mcp_stdio_servers(interactive, args)?,
            Some(McpTransportChoice::Http) => prompt_mcp_http_servers(interactive, args)?,
            Some(McpTransportChoice::Done) | None => break,
        }
    }
    Ok(())
}

fn agent_supports_skills(args: &InitArgs, registry: &RegistryCatalog) -> bool {
    let Some(agent_id) = args.agent.as_deref() else {
        return false;
    };
    registry.lookup(agent_id).is_some_and(|entry| {
        entry.supports_agent_skills && entry.agent_skills_install_dir.is_some()
    })
}

fn essential_agent_skills_available(skill_catalog: &SkillCatalog) -> bool {
    !skill_catalog
        .essential_skill_names(ANTHROPIC_SKILLS_SOURCE_ID)
        .is_empty()
        || !skill_catalog
            .essential_plugin_names(OPENAI_PLUGINS_SOURCE_ID)
            .is_empty()
}

fn apply_essential_agent_skills(args: &mut InitArgs, skill_catalog: &SkillCatalog) {
    let skills = skill_catalog.essential_skill_names(ANTHROPIC_SKILLS_SOURCE_ID);
    if !skills.is_empty() {
        args.skills_source = Some(SOURCE_ANTHROPIC.to_owned());
        args.skills = skills;
    }

    let plugins = skill_catalog.essential_plugin_names(OPENAI_PLUGINS_SOURCE_ID);
    if !plugins.is_empty() {
        args.plugins_source = Some(SOURCE_OPENAI.to_owned());
        args.plugins = plugins;
    }
}

fn prompt_data_sources(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let Some(kind) = prompt::select(
            interactive,
            "data source type",
            &[
                (
                    DataSourceKind::S3,
                    "S3 bucket".to_owned(),
                    "Bucket, region, credential refs".to_owned(),
                ),
                (
                    DataSourceKind::Https,
                    "HTTPS archive".to_owned(),
                    "Download URL".to_owned(),
                ),
                (
                    DataSourceKind::Local,
                    "Local path".to_owned(),
                    "Absolute path".to_owned(),
                ),
            ],
        )?
        else {
            break;
        };
        let Some(source) = prompt_data_source_row(interactive, kind)? else {
            break;
        };
        match prompt_setup_row_action(interactive, "Data source row")? {
            SetupRowAction::AddAnother => args.prompt_data_sources.push(source),
            SetupRowAction::Discard => continue,
            SetupRowAction::Done => {
                args.prompt_data_sources.push(source);
                break;
            }
        }
    }
    Ok(())
}

fn prompt_data_source_row(
    interactive: bool,
    kind: DataSourceKind,
) -> Result<Option<DataSourceConfig>> {
    match kind {
        DataSourceKind::Local => {
            let Some(path) = prompt::text(interactive, "local path (blank to finish)", false)?
            else {
                return Ok(None);
            };
            let path = path.trim();
            if path.is_empty() {
                return Ok(None);
            }
            classify_data_from(path).map(Some)
        }
        DataSourceKind::Https => {
            let Some(url) =
                prompt::text(interactive, "HTTPS archive URL (blank to finish)", false)?
            else {
                return Ok(None);
            };
            let url = url.trim();
            if url.is_empty() {
                return Ok(None);
            }
            classify_data_from(url).map(Some)
        }
        DataSourceKind::S3 => {
            let Some(bucket) = prompt::text(interactive, "S3 bucket (blank to finish)", false)?
            else {
                return Ok(None);
            };
            let bucket = bucket.trim().to_owned();
            if bucket.is_empty() {
                return Ok(None);
            }
            let Some(region) = prompt::text(interactive, "S3 region", true)? else {
                return Ok(None);
            };
            let region = region.trim().to_owned();
            let Some(access_key_ref) = prompt::text(
                interactive,
                "access key ref (e.g., AWS_ACCESS_KEY_ID)",
                true,
            )?
            else {
                return Ok(None);
            };
            let access_key_ref = access_key_ref.trim().to_owned();
            let Some(secret_key_ref) = prompt::text(
                interactive,
                "secret key ref (e.g., AWS_SECRET_ACCESS_KEY)",
                true,
            )?
            else {
                return Ok(None);
            };
            let secret_key_ref = secret_key_ref.trim().to_owned();
            let prefix = prompt::text(interactive, "S3 prefix (blank for bucket root)", false)?
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty());
            Ok(Some(DataSourceConfig {
                source_type: "s3".to_owned(),
                name: None,
                path: None,
                url: None,
                expected_sha256: None,
                max_download_bytes: None,
                max_extracted_bytes: None,
                bucket: Some(bucket),
                prefix,
                region: Some(region),
                access_key_ref: Some(access_key_ref),
                secret_key_ref: Some(secret_key_ref),
            }))
        }
    }
}

fn prompt_mcp_stdio_servers(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let Some(name) = prompt::text(interactive, "MCP name (blank to finish)", false)? else {
            break;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            break;
        }
        let Some(command) = prompt::text(interactive, "command", true)? else {
            break;
        };
        let command = command.trim().to_owned();
        if command.is_empty() {
            continue;
        }
        let cli_args =
            match prompt::text(interactive, "args (comma-separated, blank for none)", false)? {
                Some(raw) => parse_comma_separated_prompt_values(&raw),
                None => Vec::new(),
            };
        let env = match prompt::text(
            interactive,
            "env refs (comma-separated, blank for none)",
            false,
        )? {
            Some(raw) => parse_secret_ref_prompt_values("mcp-stdio-env", &raw)?,
            None => Vec::new(),
        };
        let row = InitMcpStdioServer {
            name,
            command,
            args: cli_args,
            env,
        };
        match prompt_setup_row_action(interactive, "MCP row")? {
            SetupRowAction::AddAnother => args.prompt_mcp_stdio.push(row),
            SetupRowAction::Discard => continue,
            SetupRowAction::Done => {
                args.prompt_mcp_stdio.push(row);
                break;
            }
        }
    }
    Ok(())
}

fn prompt_mcp_http_servers(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let Some(name) = prompt::text(interactive, "MCP name (blank to finish)", false)? else {
            break;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            break;
        }
        let Some(url) = prompt::text(interactive, "URL", true)? else {
            break;
        };
        let url = url.trim().to_owned();
        if url.is_empty() {
            continue;
        }
        let headers = match prompt::text(
            interactive,
            "headers (comma-separated Header:SECRET_REF, blank for none)",
            false,
        )? {
            Some(raw) => parse_http_header_prompt_values(&raw)?,
            None => Vec::new(),
        };
        let row = InitMcpHttpServer { name, url, headers };
        match prompt_setup_row_action(interactive, "MCP row")? {
            SetupRowAction::AddAnother => args.prompt_mcp_http.push(row),
            SetupRowAction::Discard => continue,
            SetupRowAction::Done => {
                args.prompt_mcp_http.push(row);
                break;
            }
        }
    }
    Ok(())
}

fn prompt_setup_row_action(interactive: bool, prompt_label: &str) -> Result<SetupRowAction> {
    let items = [
        (
            SetupRowAction::AddAnother,
            "Add another".to_owned(),
            "Save this row and continue".to_owned(),
        ),
        (
            SetupRowAction::Discard,
            "Discard".to_owned(),
            "Drop this row and continue".to_owned(),
        ),
        (
            SetupRowAction::Done,
            "Done".to_owned(),
            "Save this row and finish".to_owned(),
        ),
    ];
    Ok(prompt::select(interactive, prompt_label, &items)?.unwrap_or(SetupRowAction::Done))
}

fn parse_secret_ref_prompt_values(field: &'static str, raw: &str) -> Result<Vec<String>> {
    let values = parse_comma_separated_prompt_values(raw);
    for value in &values {
        if !is_valid_secret_ref_name(value) {
            return Err(StackError::InvalidParam {
                field,
                reason: format!(
                    "`{value}` is not a valid secret ref name (letters, digits, and underscore; must not start with a digit)"
                ),
            });
        }
    }
    Ok(values)
}

fn parse_http_header_prompt_values(raw: &str) -> Result<Vec<InitMcpHttpHeader>> {
    let mut out = Vec::new();
    for value in parse_comma_separated_prompt_values(raw) {
        let (name, value_ref) = split_mcp_header_ref(&value)?;
        out.push(InitMcpHttpHeader { name, value_ref });
    }
    Ok(out)
}

fn parse_comma_separated_prompt_values(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Interactive add-loop for dependency install actions. Each entry collects a
/// name, an install shell command, and whether it needs system privilege, then
/// stacks onto `--dep`/`--dep-system` so `deps_from_args` consumes it uniformly.
fn prompt_deps(interactive: bool, args: &mut InitArgs) -> Result<()> {
    loop {
        let Some(name) = prompt::text(interactive, "dependency name (blank to finish)", false)?
        else {
            break;
        };
        let name = name.trim().to_owned();
        if name.is_empty() {
            break;
        }
        let Some(shell) = prompt::text(interactive, "install shell command", true)? else {
            break;
        };
        let shell = shell.trim().to_owned();
        if shell.is_empty() {
            continue;
        }
        let entry = format!("{name}={shell}");
        let scope = prompt::select(
            interactive,
            "dependency scope",
            &[
                (
                    DependencyInstallScope::User,
                    "User".to_owned(),
                    "Runtime user install".to_owned(),
                ),
                (
                    DependencyInstallScope::System,
                    "System".to_owned(),
                    "Requires OS privilege".to_owned(),
                ),
            ],
        )?
        .unwrap_or_default();
        match scope {
            DependencyInstallScope::User => args.dep.push(entry),
            DependencyInstallScope::System => args.dep_system.push(entry),
        }
    }
    Ok(())
}

fn reject_starter_only_mcp_arg(field: &'static str, values: &[String]) -> Result<()> {
    if values.is_empty() {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field,
        reason: "MCP init declarations apply only when creating a starter config".to_owned(),
    })
}

fn reject_conflicting_deployment_override(
    field: &'static str,
    requested: Option<&str>,
    existing: &str,
) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    if requested == existing {
        return Ok(());
    }
    Err(StackError::InvalidParam {
        field,
        reason: format!(
            "deployment override applies only when creating a starter config; existing config has `{existing}`. Edit the config first or re-run with the existing value."
        ),
    })
}

pub(super) fn starter_config(args: &InitArgs) -> Result<String> {
    let workspace_root = args
        .workspace_root
        .clone()
        .unwrap_or_else(|| config::DEFAULT_WORKSPACE_ROOT.to_owned());
    let workspace_uploads = args.workspace_uploads.clone().unwrap_or_else(|| {
        if args.workspace_root.is_some() {
            Path::new(&workspace_root)
                .join("uploads")
                .display()
                .to_string()
        } else {
            config::DEFAULT_WORKSPACE_UPLOADS.to_owned()
        }
    });
    let runtime_user = starter_runtime_user(args)?;

    let agent = AgentConfig {
        id: STARTER_AGENT_ID.to_owned(),
        name: STARTER_AGENT_NAME.to_owned(),
        command: STARTER_AGENT_COMMAND.to_owned(),
        args: Vec::new(),
        cwd: Some(workspace_root.clone()),
        env: Vec::new(),
        expected_sha256: None,
        restart: STARTER_AGENT_RESTART.to_owned(),
        mode: None,
        model: None,
        harness_version: None,
        adapter: None,
        provider: None,
        subagent: None,
        auto_update: None,
        install: Some(AgentInstallConfig {
            install_type: STARTER_AGENT_INSTALL_TYPE.to_owned(),
            creates: STARTER_AGENT_INSTALL_CREATES.to_owned(),
            shell: Some(STARTER_AGENT_INSTALL_COMMAND.to_owned()),
        }),
    };
    let starter = Config {
        config_version: config::SUPPORTED_CONFIG_VERSION,
        api: ApiConfig {
            bind: config::DEFAULT_API_BIND.to_owned(),
            public_url: Some(format!("http://{}", config::DEFAULT_API_BIND)),
            max_request_bytes: STARTER_MAX_REQUEST_BYTES,
        },
        security: SecurityConfig {
            http: SecurityHttpConfig {
                max_request_bytes: STARTER_MAX_REQUEST_BYTES,
                rate_limit_per_minute: STARTER_RATE_LIMIT_PER_MINUTE,
                burst: STARTER_RATE_LIMIT_BURST,
                auth_failures_per_minute: STARTER_AUTH_FAILURES_PER_MINUTE,
                auth_block_duration: STARTER_AUTH_BLOCK_DURATION.to_owned(),
                allowed_origins: Vec::new(),
                trust_proxy_headers: false,
                trusted_proxies: Vec::new(),
            },
        },
        edge: EdgeConfig::default(),
        updates: Default::default(),
        workspace: WorkspaceConfig {
            root: workspace_root.clone(),
            uploads: workspace_uploads,
            default_shell: STARTER_DEFAULT_SHELL.to_owned(),
            runtime_user,
            max_file_bytes: STARTER_WORKSPACE_MAX_FILE_BYTES,
            code_sources: code_sources_from_args(args),
            data_sources: data_sources_from_args(args)?,
        },
        logging: LoggingConfig {
            level: STARTER_LOG_LEVEL.to_owned(),
            local_retention_days: STARTER_LOCAL_RETENTION_DAYS,
            supabase: Some(starter_supabase_config(args)),
        },
        agent: agent.clone(),
        array: config::ArrayConfig::from_agent(agent),
        permissions: Default::default(),
        commands: Default::default(),
        prompts: Default::default(),
        dependencies: Default::default(),
        mcp: mcp_from_args(args)?,
        local: Default::default(),
    };

    let canonical = starter.to_canonical_toml()?;
    config::load_config_from_str(&canonical)?;
    Ok(canonical)
}

fn starter_supabase_config(args: &InitArgs) -> SupabaseLoggingConfig {
    if args.no_supabase {
        return disabled_supabase_config();
    }
    match args.supabase_url.clone() {
        Some(url) => enabled_supabase_config(
            url,
            Some(
                args.supabase_schema
                    .clone()
                    .unwrap_or_else(|| SUPABASE_DEFAULT_SCHEMA.to_owned()),
            ),
            Some(
                args.supabase_api_key_ref
                    .clone()
                    .unwrap_or_else(|| SUPABASE_DEFAULT_API_KEY_REF.to_owned()),
            ),
        ),
        None => disabled_supabase_config(),
    }
}

fn starter_runtime_user(args: &InitArgs) -> Result<String> {
    if let Some(runtime_user) = args.runtime_user.clone() {
        return Ok(runtime_user);
    }
    if std::io::stdin().is_terminal()
        && crate::ownership::resolve_runtime_user_uid(config::DEFAULT_RUNTIME_USER)
            .map_err(|source| StackError::ServeIo { source })?
            .is_none()
        && crate::ownership::process_euid() != 0
        && let Some(current_user) =
            crate::ownership::current_username().map_err(|source| StackError::ServeIo { source })?
    {
        return Ok(current_user);
    }
    Ok(config::DEFAULT_RUNTIME_USER.to_owned())
}

fn code_sources_from_args(args: &InitArgs) -> Vec<CodeSourceConfig> {
    args.code_from
        .iter()
        .map(|repo| CodeSourceConfig {
            source_type: "git".to_owned(),
            repo: Some(repo.clone()),
            branch: None,
            credential_ref: None,
            name: None,
        })
        .collect()
}

fn data_sources_from_args(args: &InitArgs) -> Result<Vec<DataSourceConfig>> {
    let mut sources: Vec<DataSourceConfig> = args
        .data_from
        .iter()
        .map(|value| classify_data_from(value))
        .collect::<Result<_>>()?;
    sources.extend(args.prompt_data_sources.iter().cloned());
    Ok(sources)
}

fn mcp_from_args(args: &InitArgs) -> Result<McpConfig> {
    let mut servers = Vec::new();
    for preset in &args.mcp_preset {
        match preset.as_str() {
            "linear" => servers.push(McpServerConfig::Http(McpHttpServer {
                name: "linear".to_owned(),
                url: "https://mcp.linear.app/mcp".to_owned(),
                headers: vec![HttpHeaderRef {
                    name: "Authorization".to_owned(),
                    value_ref: "LINEAR_API_KEY".to_owned(),
                }],
            })),
            other => {
                return Err(StackError::InvalidParam {
                    field: "mcp-preset",
                    reason: format!("unsupported MCP preset `{other}`"),
                });
            }
        }
    }
    for value in &args.mcp_stdio {
        let (name, command) = split_mcp_pair("mcp-stdio", value)?;
        servers.push(McpServerConfig::Stdio(McpStdioServer {
            name,
            command,
            args: Vec::new(),
            env: Vec::new(),
        }));
    }
    for value in &args.prompt_mcp_stdio {
        servers.push(McpServerConfig::Stdio(McpStdioServer {
            name: value.name.clone(),
            command: value.command.clone(),
            args: value.args.clone(),
            env: value.env.clone(),
        }));
    }
    for value in &args.mcp_http {
        let (name, url) = split_mcp_pair("mcp-http", value)?;
        validate_mcp_https_url(&name, &url)?;
        servers.push(McpServerConfig::Http(McpHttpServer {
            name,
            url,
            headers: Vec::new(),
        }));
    }
    for value in &args.prompt_mcp_http {
        validate_mcp_https_url(&value.name, &value.url)?;
        servers.push(McpServerConfig::Http(McpHttpServer {
            name: value.name.clone(),
            url: value.url.clone(),
            headers: value
                .headers
                .iter()
                .map(|header| HttpHeaderRef {
                    name: header.name.clone(),
                    value_ref: header.value_ref.clone(),
                })
                .collect(),
        }));
    }
    apply_mcp_stdio_env_refs(&mut servers, &args.mcp_stdio_env)?;
    apply_mcp_http_headers(&mut servers, &args.mcp_http_header)?;
    Ok(McpConfig { servers })
}

fn apply_mcp_stdio_env_refs(servers: &mut [McpServerConfig], values: &[String]) -> Result<()> {
    for value in values {
        let (server_name, env_ref) = split_mcp_pair("mcp-stdio-env", value)?;
        let server = find_mcp_server_mut(servers, &server_name, "mcp-stdio-env")?;
        match server {
            McpServerConfig::Stdio(stdio) => stdio.env.push(env_ref),
            McpServerConfig::Http(_) => {
                return Err(StackError::InvalidParam {
                    field: "mcp-stdio-env",
                    reason: format!("MCP server `{server_name}` is not a stdio server"),
                });
            }
        }
    }
    Ok(())
}

fn apply_mcp_http_headers(servers: &mut [McpServerConfig], values: &[String]) -> Result<()> {
    for value in values {
        let (server_name, header_ref) = split_mcp_pair("mcp-http-header", value)?;
        let (header_name, value_ref) = split_mcp_header_ref(&header_ref)?;
        let server = find_mcp_server_mut(servers, &server_name, "mcp-http-header")?;
        match server {
            McpServerConfig::Http(http) => {
                if http
                    .headers
                    .iter()
                    .any(|header| header.name.eq_ignore_ascii_case(&header_name))
                {
                    return Err(StackError::InvalidParam {
                        field: "mcp-http-header",
                        reason: format!(
                            "MCP HTTP server `{server_name}` already has header `{header_name}`"
                        ),
                    });
                }
                http.headers.push(HttpHeaderRef {
                    name: header_name,
                    value_ref,
                });
            }
            McpServerConfig::Stdio(_) => {
                return Err(StackError::InvalidParam {
                    field: "mcp-http-header",
                    reason: format!("MCP server `{server_name}` is not an HTTP server"),
                });
            }
        }
    }
    Ok(())
}

fn find_mcp_server_mut<'a>(
    servers: &'a mut [McpServerConfig],
    server_name: &str,
    field: &'static str,
) -> Result<&'a mut McpServerConfig> {
    servers
        .iter_mut()
        .find(|server| server.name() == server_name)
        .ok_or_else(|| StackError::InvalidParam {
            field,
            reason: format!("MCP server `{server_name}` is not declared"),
        })
}

fn split_mcp_pair(field: &'static str, value: &str) -> Result<(String, String)> {
    let Some((name, target)) = value.split_once('=') else {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("`{value}` must use NAME=VALUE"),
        });
    };
    let name = name.trim();
    let target = target.trim();
    if name.is_empty() || target.is_empty() {
        return Err(StackError::InvalidParam {
            field,
            reason: format!("`{value}` must include a non-empty name and value"),
        });
    }
    Ok((name.to_owned(), target.to_owned()))
}

fn split_mcp_header_ref(value: &str) -> Result<(String, String)> {
    let Some((header_name, value_ref)) = value.split_once(':') else {
        return Err(StackError::InvalidParam {
            field: "mcp-http-header",
            reason: format!("`{value}` must use HEADER:SECRET_REF"),
        });
    };
    let header_name = header_name.trim();
    let value_ref = value_ref.trim();
    if header_name.is_empty() || value_ref.is_empty() {
        return Err(StackError::InvalidParam {
            field: "mcp-http-header",
            reason: format!("`{value}` must include a non-empty header and secret ref"),
        });
    }
    HeaderName::from_bytes(header_name.as_bytes()).map_err(|_| StackError::InvalidParam {
        field: "mcp-http-header",
        reason: format!("`{header_name}` is not a valid HTTP header name"),
    })?;
    Ok((header_name.to_owned(), value_ref.to_owned()))
}

fn validate_mcp_https_url(name: &str, url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| StackError::InvalidParam {
        field: "mcp-http",
        reason: format!("MCP HTTP server `{name}` URL is not valid"),
    })?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(StackError::InvalidParam {
            field: "mcp-http",
            reason: format!("MCP HTTP server `{name}` must use an https:// URL with a host"),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(StackError::InvalidParam {
            field: "mcp-http",
            reason: format!("MCP HTTP server `{name}` URL must not include credentials"),
        });
    }
    Ok(())
}

fn classify_data_from(value: &str) -> Result<DataSourceConfig> {
    if value.strip_prefix("https://").is_some() {
        reject_unsupported_https_data_source(value)?;
        return Ok(DataSourceConfig {
            source_type: "https".to_owned(),
            name: None,
            path: None,
            url: Some(value.to_owned()),
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        });
    }
    if value.starts_with("http://") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!("`{value}` must use https:// (http is not allowed)"),
        });
    }
    if !value.starts_with('/') {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!("`{value}` must be an absolute path or an https:// URL"),
        });
    }
    Ok(DataSourceConfig {
        source_type: "local".to_owned(),
        name: None,
        path: Some(value.to_owned()),
        url: None,
        expected_sha256: None,
        max_download_bytes: None,
        max_extracted_bytes: None,
        bucket: None,
        prefix: None,
        region: None,
        access_key_ref: None,
        secret_key_ref: None,
    })
}

/// Reject HTTPS data sources that the materializer cannot satisfy headlessly.
/// Catches three known failure modes BEFORE init writes any state, so the
/// operator gets a clear error pointing at the actual URL rather than a vague
/// download/extract failure halfway through materialization.
///
/// Patterns rejected:
/// - `drive.google.com/file/d/.../view` (private file view link; needs the
///   `uc?export=download&id=` form to expose a usable HTTPS download)
/// - `drive.google.com/drive/folders/...` (folder, not an archive; the
///   materializer downloads single files)
/// - `dropbox.com/.../?dl=0` or no `dl` param (preview link; needs `?dl=1`)
fn reject_unsupported_https_data_source(value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("drive.google.com/file/d/")
        && !lower.contains("uc?export=download")
        && !lower.contains("uc?id=")
    {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a private Drive file viewer link; pass the `https://drive.google.com/uc?export=download&id=<ID>` form instead"
            ),
        });
    }
    if lower.contains("drive.google.com/drive/folders/") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a Drive folder; init only supports single-archive downloads. Export the folder as an archive and link to the archive."
            ),
        });
    }
    if lower.contains("dropbox.com/") && !lower.contains("dl=1") && !lower.contains("raw=1") {
        return Err(StackError::InvalidParam {
            field: "data-from",
            reason: format!(
                "`{value}` is a Dropbox preview link; append `?dl=1` so the materializer receives the file bytes"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[derive(Debug, Parser)]
    struct TestInitArgs {
        #[command(flatten)]
        args: InitArgs,
    }

    fn parse_init_args(args: &[&str]) -> InitArgs {
        let mut argv = vec!["init-test"];
        argv.extend_from_slice(args);
        TestInitArgs::parse_from(argv).args
    }

    fn starter_config_from_args(args: &InitArgs) -> Config {
        let raw = starter_config(args).expect("starter config");
        config::load_config_from_str(&raw).expect("starter config validates")
    }

    fn collection(fresh: &[(&str, &str)]) -> AgentEnvCollection {
        AgentEnvCollection {
            flag_refs: Vec::new(),
            fresh: fresh
                .iter()
                .map(|(name, value)| {
                    (
                        (*name).to_owned(),
                        zeroize::Zeroizing::new((*value).to_owned()),
                    )
                })
                .collect(),
        }
    }

    // A fresh agent-env name that collides with a secret already in the store
    // must be rejected before the upsert, leaving the existing secret untouched.
    #[test]
    fn apply_agent_env_refuses_to_overwrite_existing_secret() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");
        store
            .set("ADMIN_KEY", "original-admin-secret")
            .expect("seed");

        let error =
            apply_agent_env_collection(&mut store, &collection(&[("ADMIN_KEY", "attacker")]))
                .expect_err("collision with an existing secret must be rejected");
        assert!(error.to_string().contains("already exists"), "got: {error}");
        assert_eq!(
            store.get("ADMIN_KEY").expect("preserved"),
            "original-admin-secret",
            "the existing secret must not be overwritten"
        );
    }

    #[test]
    fn apply_agent_env_rejects_invalid_ref_name() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");

        let error = apply_agent_env_collection(&mut store, &collection(&[("bad-name", "v")]))
            .expect_err("an invalid ref name must be rejected");
        assert!(
            error.to_string().contains("valid secret ref name"),
            "got: {error}"
        );
    }

    #[test]
    fn apply_agent_env_stores_a_new_secret() {
        let home = tempdir().expect("tempdir");
        let mut store = SecretStore::open_or_create(home.path()).expect("store");

        apply_agent_env_collection(&mut store, &collection(&[("GITHUB_TOKEN", "ghp_value")]))
            .expect("a new, valid ref should be stored");
        assert_eq!(store.get("GITHUB_TOKEN").expect("stored"), "ghp_value");
    }

    // Scripts the hosted-prompt driver so the interactive environment-config flow
    // can be exercised headlessly: `selects`/`confirms` are dequeued in call order,
    // and text/password return None so any add-loop finishes immediately.
    struct ScriptedPromptDriver {
        selects: Mutex<VecDeque<Option<usize>>>,
        confirms: Mutex<VecDeque<bool>>,
    }

    impl ScriptedPromptDriver {
        fn new(selects: Vec<Option<usize>>, confirms: Vec<bool>) -> Self {
            Self {
                selects: Mutex::new(VecDeque::from(selects)),
                confirms: Mutex::new(VecDeque::from(confirms)),
            }
        }
    }

    impl prompt::HostedPromptDriver for ScriptedPromptDriver {
        fn select(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
            Ok(prompt::HostedPromptOutcome::Handled(
                self.selects
                    .lock()
                    .expect("selects lock")
                    .pop_front()
                    .expect("scripted select"),
            ))
        }

        fn confirm(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<bool>> {
            Ok(prompt::HostedPromptOutcome::Handled(
                self.confirms
                    .lock()
                    .expect("confirms lock")
                    .pop_front()
                    .expect("scripted confirm"),
            ))
        }

        fn text(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
            Ok(prompt::HostedPromptOutcome::Handled(None))
        }

        fn password(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
            Ok(prompt::HostedPromptOutcome::Handled(None))
        }

        fn progress(&self, _message: String) {}

        fn result(&self, _payload: serde_json::Value) {}
    }

    // Models a hosted driver that leaves the environment-config prompt outside its
    // v1 scope: every prompt is Unhandled, so the flow must skip cleanly.
    struct UnhandledPromptDriver;

    impl prompt::HostedPromptDriver for UnhandledPromptDriver {
        fn select(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
        }

        fn confirm(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<bool>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
        }

        fn text(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
        }

        fn password(
            &self,
            _request: prompt::HostedPromptRequest,
        ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
            Ok(prompt::HostedPromptOutcome::Unhandled)
        }

        fn progress(&self, _message: String) {}

        fn result(&self, _payload: serde_json::Value) {}
    }

    fn run_environment_configuration(
        driver: Arc<dyn prompt::HostedPromptDriver>,
        args: &mut InitArgs,
    ) -> Result<()> {
        let registry = RegistryCatalog::load_embedded().expect("registry");
        let skill_catalog = SkillCatalog::load_embedded().expect("skill catalog");
        prompt::with_hosted_driver(driver, || {
            prompt_environment_configuration_if_needed(args, &registry, &skill_catalog)
        })
    }

    // Standard Setup (path index 0): essential deps + browser-use accepted,
    // skills skipped for a non-skills agent, data declined. It must touch none
    // of the Advanced-only seams and must make exactly one select (the path
    // choice) — extra selects would drain the single-item queue and panic.
    #[test]
    fn standard_setup_enables_essential_deps_and_browser_use() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(0)],
            vec![true, true, false],
        ));
        let mut args = parse_init_args(&["--agent", "placebo"]);

        run_environment_configuration(driver, &mut args).expect("standard setup");

        assert!(args.standard_agent_work_deps);
        assert!(args.browser_use_profile);
        assert!(!args.prompt_skills);
        assert!(!args.prompt_agent_env_refs);
        assert!(args.prompt_data_sources.is_empty());
    }

    // Standard Setup with every prompt declined enables nothing.
    #[test]
    fn standard_setup_decline_all_enables_nothing() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(0)],
            vec![false, false, false],
        ));
        let mut args = parse_init_args(&["--agent", "placebo"]);

        run_environment_configuration(driver, &mut args).expect("standard setup");

        assert!(!args.standard_agent_work_deps);
        assert!(!args.browser_use_profile);
        assert!(!args.prompt_skills);
        assert!(args.prompt_data_sources.is_empty());
    }

    #[test]
    fn standard_setup_adds_essential_skills_for_skills_capable_agent() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(0)],
            vec![false, false, true, false],
        ));
        let mut args = parse_init_args(&["--agent", "opencode"]);

        run_environment_configuration(driver, &mut args).expect("standard setup");

        assert_eq!(args.skills_source.as_deref(), Some(SOURCE_ANTHROPIC));
        assert_eq!(args.skills, ["docx", "pptx", "xlsx", "pdf"]);
        assert_eq!(args.plugins_source.as_deref(), Some(SOURCE_OPENAI));
        assert_eq!(args.plugins, ["github"]);
        assert!(!args.prompt_skills);
        assert!(args.prompt_data_sources.is_empty());
    }

    #[test]
    fn standard_setup_keeps_explicit_skill_flags() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(0)],
            vec![false, false, false],
        ));
        let mut args = parse_init_args(&[
            "--agent",
            "opencode",
            "--skills-source",
            "anthropic",
            "--skills",
            "docx",
        ]);

        run_environment_configuration(driver, &mut args).expect("standard setup");

        assert_eq!(args.skills_source.as_deref(), Some(SOURCE_ANTHROPIC));
        assert_eq!(args.skills, ["docx"]);
        assert!(args.plugins_source.is_none());
        assert!(args.plugins.is_empty());
    }

    // Advanced Setup (path index 1) with a non-skills agent: deps off, MCP off,
    // agent env on, data off. `placebo` is absent from the embedded registry, so
    // `agent_supports_skills` is false and the skills prompt is skipped — hence
    // four confirms, not five.
    #[test]
    fn advanced_setup_routes_agent_env_without_standard_fields() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(1)],
            vec![false, false, true, false],
        ));
        let mut args = parse_init_args(&["--agent", "placebo"]);

        run_environment_configuration(driver, &mut args).expect("advanced setup");

        assert!(args.prompt_agent_env_refs);
        assert!(!args.prompt_skills);
        assert!(!args.standard_agent_work_deps);
        assert!(!args.browser_use_profile);
    }

    // Advanced Setup offers the skills step only when the agent supports skills;
    // `opencode` does, so accepting it routes into the skills flow.
    #[test]
    fn advanced_setup_routes_agent_skills_for_skills_capable_agent() {
        let driver = Arc::new(ScriptedPromptDriver::new(
            vec![Some(1)],
            vec![false, true, false, false, false],
        ));
        let mut args = parse_init_args(&["--agent", "opencode"]);

        run_environment_configuration(driver, &mut args).expect("advanced setup");

        assert!(args.prompt_skills);
        assert!(!args.prompt_agent_env_refs);
    }

    // A hosted driver that leaves the path prompt Unhandled skips environment
    // configuration instead of failing, matching non-interactive behavior.
    #[test]
    fn unhandled_hosted_prompt_skips_environment_configuration() {
        let driver = Arc::new(UnhandledPromptDriver);
        let mut args = parse_init_args(&["--agent", "placebo"]);

        run_environment_configuration(driver, &mut args).expect("skip is not an error");

        assert!(!args.standard_agent_work_deps);
        assert!(!args.browser_use_profile);
        assert!(!args.prompt_skills);
        assert!(!args.prompt_agent_env_refs);
    }

    #[test]
    fn standard_setup_profile_declares_base_dependencies_without_build_toolchain() {
        let mut args = parse_init_args(&["--agent", "placebo"]);
        args.standard_agent_work_deps = true;
        let mut config = starter_config_from_args(&args);

        push_args_deps_to_config(&mut config, &args).expect("push standard deps");

        let bundle = config
            .dependencies
            .commands
            .iter()
            .find(|entry| entry.name == STANDARD_AGENT_WORK_BUNDLE_NAME)
            .expect("standard bundle dependency");
        let install = bundle.install.as_ref().expect("bundle install action");
        assert_eq!(bundle.feature.as_deref(), Some(STANDARD_AGENT_WORK_FEATURE));
        assert_eq!(install.scope, DependencyInstallScope::System);
        assert_eq!(
            install.creates.as_deref(),
            Some(STANDARD_AGENT_WORK_BUNDLE_CREATES)
        );
        assert!(install.shell.contains("apt-get install"));
        assert!(
            install
                .shell
                .contains("UV_PYTHON_INSTALL_DIR=/opt/acp-stack/python UV_PYTHON_BIN_DIR=/usr/local/bin uv python install 3.14"),
            "{}",
            install.shell
        );

        for command in [
            "node",
            "npm",
            "python3",
            "python3.14",
            "uv",
            "git",
            "rg",
            "jq",
        ] {
            assert!(
                config
                    .dependencies
                    .commands
                    .iter()
                    .any(|entry| entry.name == command
                        && entry.feature.as_deref() == Some(STANDARD_AGENT_WORK_FEATURE)
                        && entry.install.is_none()),
                "missing command dependency {command}"
            );
        }
        for package in STANDARD_AGENT_WORK_APT_PACKAGES {
            assert!(
                config
                    .dependencies
                    .packages
                    .iter()
                    .any(|entry| entry.name == *package
                        && entry.feature.as_deref() == Some(STANDARD_AGENT_WORK_FEATURE)),
                "missing package dependency {package}"
            );
        }
        for package in BUILD_HEAVY_APT_PACKAGES {
            assert!(
                !config
                    .dependencies
                    .packages
                    .iter()
                    .any(|entry| entry.name == *package),
                "standard setup must not include {package}"
            );
            assert!(
                !install.shell.contains(package),
                "standard install shell must not include {package}"
            );
        }

        let canonical = config.to_canonical_toml().expect("canonical config");
        config::load_config_from_str(&canonical).expect("canonical config validates");
    }

    #[test]
    fn browser_use_profile_declares_dependency_without_generic_mcp_prompt_config() {
        let mut args = parse_init_args(&["--agent", "placebo"]);
        args.browser_use_profile = true;
        let mut config = starter_config_from_args(&args);

        push_args_deps_to_config(&mut config, &args).expect("push browser deps");

        let browser = config
            .dependencies
            .commands
            .iter()
            .find(|entry| entry.name == BROWSER_USE_MCP_COMMAND)
            .expect("browser-use launcher dependency");
        assert_eq!(browser.feature.as_deref(), Some(BROWSER_USE_FEATURE));
        let install = browser.install.as_ref().expect("browser install action");
        assert_eq!(install.scope, DependencyInstallScope::System);
        assert_eq!(install.creates.as_deref(), Some(BROWSER_USE_MCP_COMMAND));
        for required in [
            "apt-get install",
            "chromium",
            "chromium-browser",
            "uv venv --python 3.14",
            "browser-use[core]",
            BROWSER_USE_PREFIX,
            BROWSER_USE_WRAPPER_PATH,
            BROWSER_USE_LAUNCHER_PATH,
            "FastMCP",
            "BROWSER_USE_API_KEY",
            "BROWSER_USE_VENV=\"${BROWSER_USE_VENV:-/opt/acp-stack/browser-use}\"",
            "BROWSER_USE_MCP_SCRIPT=\"${BROWSER_USE_MCP_SCRIPT:-/usr/local/share/acp-stack/browser-use-mcp.py}\"",
            "exec \"${BROWSER_USE_VENV}/bin/python\"",
        ] {
            assert!(
                install.shell.contains(required),
                "browser install shell must include {required}"
            );
        }
        assert!(config.mcp.servers.is_empty());

        let canonical = config.to_canonical_toml().expect("canonical config");
        config::load_config_from_str(&canonical).expect("canonical config validates");
    }
}
