use super::super::*;
use crate::runtime::install::agent_registry::{
    AdapterSpec, HarnessSpec, InstallProvidedBy, ShellInstall, default_acp_args,
};
use crate::state::StateStore;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

pub(crate) fn open_store() -> (TempDir, StateStore) {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("open");
    store.migrate().expect("migrate");
    (tempdir, store)
}

pub(crate) fn install_config(shell: &str, creates: &str) -> AgentInstallConfig {
    AgentInstallConfig {
        install_type: "shell".into(),
        creates: creates.into(),
        shell: Some(shell.into()),
    }
}

pub(crate) fn workspace_root() -> PathBuf {
    std::env::temp_dir()
}

pub(crate) fn agent_config(command: &str) -> AgentConfig {
    AgentConfig {
        id: "test-agent".to_owned(),
        name: "Test Agent".to_owned(),
        command: command.to_owned(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        expected_sha256: None,
        restart: "on-crash".to_owned(),
        mode: None,
        model: None,
        harness_version: None,
        adapter: None,
        provider: None,
        providers: None,
        subagent: None,
        auto_update: None,
        install: None,
    }
}

pub(crate) fn shell_install_set(script: &str, creates: &str) -> InstallSet {
    InstallSet {
        shell: Some(ShellInstall {
            script: script.to_owned(),
            creates: creates.to_owned(),
            required_tools: Vec::new(),
            timeout_secs: None,
        }),
        ..InstallSet::default()
    }
}

pub(crate) fn shell_install_set_with_timeout(
    script: &str,
    creates: &str,
    timeout_secs: u64,
) -> InstallSet {
    InstallSet {
        shell: Some(ShellInstall {
            script: script.to_owned(),
            creates: creates.to_owned(),
            required_tools: Vec::new(),
            timeout_secs: Some(timeout_secs),
        }),
        ..InstallSet::default()
    }
}

pub(crate) fn adapter_provided_install_set() -> InstallSet {
    InstallSet {
        provided_by: Some(InstallProvidedBy::Adapter),
        ..InstallSet::default()
    }
}

pub(crate) fn harness_spec(id: &str, install: InstallSet) -> HarnessSpec {
    HarnessSpec {
        id: id.to_owned(),
        acp_args: default_acp_args(),
        install,
        update: Default::default(),
    }
}

pub(crate) fn adapter_spec(id: &str, install: InstallSet) -> AdapterSpec {
    AdapterSpec {
        id: id.to_owned(),
        sync_id: None,
        github: None,
        install,
        update: Default::default(),
    }
}

pub(crate) fn native_entry(
    id: &str,
    name: &str,
    support_doc: Option<&str>,
    harness: HarnessSpec,
) -> RegistryEntry {
    RegistryEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        kind: RegistryKind::Native,
        headless_compatible: support_doc.is_some(),
        set_provider: false,
        multiple_active_providers: false,
        set_model: false,
        set_mode: false,
        supports_agent_skills: false,
        agent_skills_install_dir: None,
        agent_skills_link_dir: None,
        subagents: false,
        subagent_alias: None,
        subagent_free_models: Vec::new(),
        sync_exempt: false,
        sync_id: None,
        allow_custom_provider: false,
        set_provider_base_url: false,
        allow_custom_model: false,
        stdio_framing: Default::default(),
        website: None,
        github: None,
        support_doc: support_doc.map(str::to_owned),
        testflight_prompt: None,
        testflight_expect_fs: None,
        adapter: None,
        harness: Some(harness),
    }
}

pub(crate) fn adapter_entry(
    id: &str,
    name: &str,
    support_doc: Option<&str>,
    harness: HarnessSpec,
    adapter: AdapterSpec,
) -> RegistryEntry {
    RegistryEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        kind: RegistryKind::Adapter,
        headless_compatible: support_doc.is_some(),
        set_provider: false,
        multiple_active_providers: false,
        set_model: false,
        set_mode: false,
        supports_agent_skills: false,
        agent_skills_install_dir: None,
        agent_skills_link_dir: None,
        subagents: false,
        subagent_alias: None,
        subagent_free_models: Vec::new(),
        sync_exempt: false,
        sync_id: None,
        allow_custom_provider: false,
        set_provider_base_url: false,
        allow_custom_model: false,
        stdio_framing: Default::default(),
        website: None,
        github: None,
        support_doc: support_doc.map(str::to_owned),
        testflight_prompt: None,
        testflight_expect_fs: None,
        adapter: Some(adapter),
        harness: Some(harness),
    }
}

// Fixture binaries carry a shebang so they pass the installer's spawn gate;
// `content` lands after it as a `#`-prefixed comment to keep files distinct
// without the probe executing it.
pub(crate) fn shell_string_for_write(path: &Path, content: &str) -> String {
    format!(
        "mkdir -p {bin} && printf '#!/bin/sh\\n# %s' {content} > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(path.parent().expect("binary has parent")),
        content = shell_quote_literal(content),
        binary = shell_quote_path(path),
    )
}

pub(crate) fn shell_quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub(crate) fn write_fake_npm(dest_dir: &Path, body: &str) {
    let npm_path = dest_dir.join("npm");
    std::fs::write(&npm_path, format!("#!/bin/sh\n{body}")).expect("write fake npm");
    let permissions = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(&npm_path, permissions).expect("chmod fake npm");
}

// Mirrors the blocked-postinstall stub npm 12 leaves behind: executable file,
// no shebang, so exec fails with ENOEXEC despite `creates` resolving.
pub(crate) fn shell_string_for_stub_write(path: &Path) -> String {
    format!(
        "mkdir -p {bin} && printf 'not a real binary' > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(path.parent().expect("binary has parent")),
        binary = shell_quote_path(path),
    )
}

pub(crate) fn shell_quote_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}
