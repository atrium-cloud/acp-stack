use super::*;

pub(super) const STANDARD_AGENT_WORK_FEATURE: &str = "agent-work";
pub(super) const STANDARD_AGENT_WORK_BUNDLE_NAME: &str = "acp-stack-agent-work-base";
pub(super) const STANDARD_AGENT_WORK_BUNDLE_CREATES: &str =
    "/usr/local/share/acp-stack/agent-work-base.done";
pub(super) const BROWSER_USE_FEATURE: &str = "browser";
pub(super) const BROWSER_USE_MCP_COMMAND: &str = "acp-stack-browser-use-mcp";
pub(super) const AGENT_WORK_PYTHON_VERSION: &str = "3.14";
pub(super) const AGENT_WORK_PYTHON_INSTALL_DIR: &str = "/opt/acp-stack/python";
pub(super) const BROWSER_USE_PYTHON_VERSION: &str = "3.14";
pub(super) const BROWSER_USE_PREFIX: &str = "/opt/acp-stack/browser-use";
pub(super) const BROWSER_USE_SHARE_DIR: &str = "/usr/local/share/acp-stack";
pub(super) const BROWSER_USE_WRAPPER_PATH: &str = "/usr/local/share/acp-stack/browser-use-mcp.py";
pub(super) const BROWSER_USE_LAUNCHER_PATH: &str = "/usr/local/bin/acp-stack-browser-use-mcp";

// Centralized package manifest for init's Standard Setup path. This mirrors the
// VM base profile: broad agent-work tools, no build toolchains or language
// headers, and no inferred package-manager behavior.
pub(super) const STANDARD_AGENT_WORK_APT_PACKAGES: &[&str] = &[
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

pub(super) const STANDARD_AGENT_WORK_COMMANDS: &[&str] = &[
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

pub(super) const BROWSER_USE_APT_PACKAGES: &[&str] = &[
    "ca-certificates",
    "curl",
    "fonts-noto",
    "fonts-noto-color-emoji",
    "fonts-noto-cjk",
    "fonts-liberation",
    "fonts-dejavu",
    "fonts-freefont-ttf",
];

pub(super) const BUILD_HEAVY_APT_PACKAGES: &[&str] =
    &["build-essential", "pkg-config", "python3-dev"];

/// Operator-supplied agent environment variable references collected during
/// init. `flag_refs` (from `--agent-env-ref`) must already exist in the secret
/// store; `fresh` holds interactively-entered name+value pairs to write.
/// Values are `Zeroizing` and never echoed or recorded in the init run args.
#[derive(Default)]
pub(crate) struct AgentEnvCollection {
    pub(super) flag_refs: Vec<String>,
    pub(super) fresh: Vec<(String, zeroize::Zeroizing<String>)>,
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
pub(crate) fn reject_agent_env_refs_for_existing_config(args: &InitArgs) -> Result<()> {
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
pub(crate) fn collect_agent_env_refs_for_init(
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
pub(crate) fn append_agent_env_refs(config: &mut Config, collection: &AgentEnvCollection) -> bool {
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
pub(crate) fn apply_agent_env_collection(
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
    include_str!("../../../../scripts/browser-use-mcp")
        .replace("@BROWSER_USE_VENV@", BROWSER_USE_PREFIX)
        .replace("@BROWSER_USE_MCP_SCRIPT@", BROWSER_USE_WRAPPER_PATH)
}

fn browser_use_install_shell() -> String {
    let packages = BROWSER_USE_APT_PACKAGES.join(" ");
    let launcher_script = browser_use_launcher_script();
    let wrapper_script = include_str!("../../../../scripts/browser-use-mcp.py");
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
pub(crate) fn push_args_deps_to_config(config: &mut Config, args: &InitArgs) -> Result<()> {
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
pub(crate) fn reject_deps_args_for_existing_config(args: &InitArgs) -> Result<()> {
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
pub(crate) fn should_apply_deps_for_init(
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
