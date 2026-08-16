use super::super::*;
use super::support::*;
use crate::runtime::install::agent_registry::ShellInstall;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[test]
fn spawn_gate_fails_step_on_unrunnable_binary() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let binary_path = dest_dir.join("stub-agent");
    let entry = native_entry(
        "stub-agent",
        "Stub Agent",
        Some("docs/agents/stub-agent.md"),
        harness_spec(
            "stub-agent",
            shell_install_set(&shell_string_for_stub_write(&binary_path), "stub-agent"),
        ),
    );

    let result = install_resolved_capture(
        &agent_config("stub-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    let err = result
        .outcome
        .expect_err("shebang-less stub must fail the spawn gate");
    assert!(
        matches!(err, StackError::AgentInstallerBinaryUnrunnable { .. }),
        "expected AgentInstallerBinaryUnrunnable, got {err:?}",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "failed");
}

#[test]
fn spawn_gate_failure_advances_fallback_chain_to_npm() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    // Shell path drops a stub for a different name than npm's `creates` so the
    // npm step's postcheck cannot accidentally resolve the shell leftovers.
    let stub_path = dest_dir.join("chain-agent-stub");
    let shell_script = format!(
        "{write} && cp {stub} {creates_path}",
        write = shell_string_for_stub_write(&stub_path),
        stub = shell_quote_path(&stub_path),
        creates_path = shell_quote_path(&dest_dir.join("chain-agent")),
    );
    write_fake_npm(
        &dest_dir,
        r#"
set -eu
if [ "$1" = "view" ]; then
  printf '"1.2.3"\n'
  exit 0
fi
if [ "$1" = "install" ]; then
  mkdir -p "$4/bin"
  printf '#!/bin/sh\n' > "$4/bin/chain-agent"
  chmod 755 "$4/bin/chain-agent"
  exit 0
fi
exit 99
"#,
    );
    let install = InstallSet {
        shell: Some(ShellInstall {
            script: shell_script,
            creates: "chain-agent".to_owned(),
            required_tools: Vec::new(),
        }),
        npm: Some(crate::runtime::install::agent_registry::NpmInstall {
            package: "chain-agent".to_owned(),
            creates: "chain-agent".to_owned(),
        }),
        ..InstallSet::default()
    };
    let entry = native_entry(
        "chain-agent",
        "Chain Agent",
        Some("docs/agents/chain-agent.md"),
        harness_spec("chain-agent", install),
    );

    let result = install_resolved_capture(
        &agent_config("chain-agent"),
        &entry,
        HashMap::new(),
        tempdir.path(),
        &dest_dir,
    );

    result
        .outcome
        .expect("npm fallback should replace the stub the shell path produced");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].status, "failed");
    assert_eq!(result.rows[0].method.as_deref(), Some("shell"));
    assert_eq!(result.rows[1].status, "ran");
    assert_eq!(result.rows[1].method.as_deref(), Some("npm"));
}

#[test]
fn executable_header_check_accepts_known_formats_and_rejects_text() {
    let tempdir = TempDir::new().expect("tempdir");
    let case = |name: &str, bytes: &[u8]| {
        let path = tempdir.path().join(name);
        std::fs::write(&path, bytes).expect("write header fixture");
        verify_executable_header(&path)
    };
    assert!(case("elf", b"\x7fELF\x02\x01\x01\x00").is_ok());
    assert!(case("shebang", b"#!/bin/sh\n").is_ok());
    assert!(case("macho", &[0xcf, 0xfa, 0xed, 0xfe, 0x00, 0x00]).is_ok());
    assert!(case("fat-macho", &[0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00]).is_ok());
    assert!(case("fat-macho-64", &[0xca, 0xfe, 0xba, 0xbf, 0x00, 0x00]).is_ok());
    assert!(case("fat-macho-64-cigam", &[0xbf, 0xba, 0xfe, 0xca, 0x00, 0x00]).is_ok());
    assert!(case("empty", b"").is_err());
    assert!(case("short-text", b"ok").is_err());
    assert!(case("stub", b"echo \"Error: postinstall was not run.\"").is_err());
}

#[test]
fn escape_hatch_reinstalls_over_unrunnable_existing_binary() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary = tempdir.path().join("hatch-agent");
    std::fs::write(&binary, b"not a real binary").expect("write stub");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    let script = format!(
        "printf '#!/bin/sh\\n' > {binary} && chmod 755 {binary}",
        binary = shell_quote_path(&binary),
    );
    let install = install_config(&script, binary.to_str().expect("utf8 tempdir path"));

    let result = run_installer_capture(&install, None, HashMap::new(), tempdir.path());

    match result
        .outcome
        .expect("recipe should replace the unrunnable pre-existing binary")
    {
        InstallerOutcome::Installed { path, .. } => assert_eq!(path, binary),
        other => panic!("expected Installed after reinstall, got {other:?}"),
    }
}

#[test]
fn init_resume_verifier_rejects_unrunnable_binary() {
    let tempdir = TempDir::new().expect("tempdir");
    let workspace_root = tempdir.path().join("workspace");
    std::fs::create_dir_all(workspace_root.join("bin")).expect("workspace bin");
    let stub = workspace_root.join("bin/stub-agent");
    std::fs::write(&stub, b"not a real binary").expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");

    assert_eq!(
        resolve_creates_for_init_resume("bin/stub-agent", &workspace_root, &[], None),
        None,
        "a resolvable but unspawnable binary must read as absent so resume re-installs",
    );
}

#[test]
fn init_resume_verifier_enforces_pin_before_probing() {
    let tempdir = TempDir::new().expect("tempdir");
    let workspace_root = tempdir.path().join("workspace");
    std::fs::create_dir_all(workspace_root.join("bin")).expect("workspace bin");
    let binary = workspace_root.join("bin/pinned-agent");
    std::fs::write(&binary, b"#!/bin/sh\n").expect("write binary");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    assert_eq!(
        resolve_creates_for_init_resume("bin/pinned-agent", &workspace_root, &[], Some("deadbeef"),),
        None,
        "a binary failing the operator's pin must read as absent so resume re-installs",
    );
    let sha256 = sha256_of_file(&binary).expect("hash binary");
    assert_eq!(
        resolve_creates_for_init_resume("bin/pinned-agent", &workspace_root, &[], Some(&sha256)),
        Some(binary),
        "a binary matching its pin is probed and accepted",
    );
}

#[test]
fn declared_pin_keeps_step_gate_from_executing_binary() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    let binary_path = dest_dir.join("pin-agent");
    // The installed script passes the header check (`#!`) but a spawn probe
    // would deterministically fail: the interpreter does not exist. So if the
    // step-level gate ever regressed to probing under a declared pin, the
    // outcome would be AgentInstallerBinaryUnrunnable instead of the pin
    // mismatch — the assertion below proves the probe never ran.
    let script = format!(
        "mkdir -p {bin} && printf '#!/nonexistent/acp-stack-test-interpreter\\nexit 0\\n' > {binary} && chmod 755 {binary}",
        bin = shell_quote_path(&dest_dir),
        binary = shell_quote_path(&binary_path),
    );
    let entry = native_entry(
        "pin-agent",
        "Pin Agent",
        Some("docs/agents/pin-agent.md"),
        harness_spec("pin-agent", shell_install_set(&script, "pin-agent")),
    );
    let mut agent = agent_config("pin-agent");
    agent.expected_sha256 = Some("deadbeef".to_owned());

    let result =
        install_resolved_capture(&agent, &entry, HashMap::new(), tempdir.path(), &dest_dir);

    let err = result
        .outcome
        .expect_err("a mismatched pin must fail final verification");
    assert!(
        matches!(err, StackError::AgentSha256Mismatch { .. }),
        "expected AgentSha256Mismatch (pin checked before any probe), got {err:?}",
    );
}

#[test]
fn declared_pin_step_gate_still_rejects_shebang_less_stub() {
    let tempdir = TempDir::new().expect("tempdir");
    let dest_dir = tempdir.path().join("bin");
    std::fs::create_dir(&dest_dir).expect("create bin dir");
    let binary_path = dest_dir.join("pin-stub-agent");
    let entry = native_entry(
        "pin-stub-agent",
        "Pin Stub Agent",
        Some("docs/agents/pin-stub-agent.md"),
        harness_spec(
            "pin-stub-agent",
            shell_install_set(&shell_string_for_stub_write(&binary_path), "pin-stub-agent"),
        ),
    );
    let mut agent = agent_config("pin-stub-agent");
    agent.expected_sha256 = Some("deadbeef".to_owned());

    let result =
        install_resolved_capture(&agent, &entry, HashMap::new(), tempdir.path(), &dest_dir);

    let err = result
        .outcome
        .expect_err("the header-only step gate must still reject a stub under a declared pin");
    assert!(
        matches!(err, StackError::AgentInstallerBinaryUnrunnable { .. }),
        "expected AgentInstallerBinaryUnrunnable, got {err:?}",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].status, "failed");
}

#[test]
fn spawn_gate_probe_fails_on_missing_interpreter() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary = tempdir.path().join("bad-interp-agent");
    std::fs::write(
        &binary,
        b"#!/nonexistent/acp-stack-test-interpreter\nexit 0\n",
    )
    .expect("write bad-interpreter script");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
        .expect("chmod bad-interpreter script");

    let err = verify_binary_spawns(&binary, tempdir.path(), &[])
        .expect_err("a script whose interpreter is missing cannot spawn");
    assert!(
        matches!(err, StackError::AgentInstallerBinaryUnrunnable { .. }),
        "expected AgentInstallerBinaryUnrunnable, got {err:?}",
    );
}

#[test]
fn spawn_gate_probe_runs_exec_only_binary_when_header_read_is_denied() {
    let tempdir = TempDir::new().expect("tempdir");
    let binary = tempdir.path().join("exec-only-agent");
    std::fs::write(&binary, b"#!/bin/sh\n").expect("write exec-only script");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o111))
        .expect("chmod exec-only script");
    if std::fs::File::open(&binary).is_ok() {
        // Root can read a mode-0111 file, so there is no denied read for the
        // header check to skip on; the scenario this test covers is absent.
        return;
    }

    verify_binary_spawns(&binary, tempdir.path(), &[])
        .expect("an unreadable-but-executable script must pass via the spawn probe");
}
