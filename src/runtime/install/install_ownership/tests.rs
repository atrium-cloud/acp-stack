use super::*;
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::state::{INSTALLER_OPERATION_INSTALL, InstallerRunInput};

const REGISTRY: &str = r#"
[[agents]]
id = "solo"
name = "Solo"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/solo.md"

[agents.harness]
id = "ownership-test-solo"

[agents.harness.install.npm]
package = "@example/solo"
creates = "ownership-test-solo"

[[agents]]
id = "duo"
name = "Duo"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/duo.md"

[agents.adapter]
id = "ownership-test-duo-acp"

[agents.adapter.install.npm]
package = "@example/duo-acp"
creates = "ownership-test-duo-acp"

[agents.harness]
id = "ownership-test-duo"

[agents.harness.install.npm]
package = "@example/duo"
creates = "ownership-test-duo"

[[agents]]
id = "bundled"
name = "Bundled"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/bundled.md"

[agents.adapter]
id = "ownership-test-bundled-acp"

[agents.adapter.install.npm]
package = "@example/bundled-acp"
creates = "ownership-test-bundled-acp"

[agents.harness]
id = "ownership-test-bundled-sdk"

[agents.harness.install]
provided_by = "adapter"
"#;

fn agent(id: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_owned(),
        name: id.to_owned(),
        command: id.to_owned(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        expected_sha256: None,
        restart: "never".to_owned(),
        mode: None,
        model: None,
        effort: None,
        config_options: Default::default(),
        harness_version: None,
        adapter: None,
        adapter_override: None,
        provider: None,
        providers: None,
        subagent: None,
        auto_update: None,
        install: None,
    }
}

fn components(id: &str) -> Vec<InstallComponent> {
    let registry = RegistryCatalog::from_toml(REGISTRY).expect("registry");
    let entry = registry.lookup_required(id).expect("entry");
    install_components(&agent(id), entry).expect("components")
}

struct Fixture {
    home: tempfile::TempDir,
    store: StateStore,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home");
        let store = StateStore::open(home.path().join("state.sqlite")).expect("state");
        store.migrate().expect("migrate");
        Self { home, store }
    }

    fn bin_dir(&self) -> std::path::PathBuf {
        let dir = crate::runtime::install::local_bin_dir(self.home.path());
        std::fs::create_dir_all(&dir).expect("bin dir");
        dir
    }

    fn write_binary(&self, path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("binary dir");
        std::fs::write(path, format!("#!/bin/sh\necho {body}\n")).expect("binary");
    }

    fn record(&self, status: &str, step: &str, path: &Path) {
        let artifact = InstalledArtifact::of(path).expect("artifact");
        let path = path.display().to_string();
        self.store
            .append_installer_run(InstallerRunInput {
                agent_id: "duo",
                started_at: "2026-09-26T00:00:00.000000000Z",
                finished_at: Some("2026-09-26T00:00:01.000000000Z"),
                status,
                stdout: "",
                stderr: "",
                exit_status: Some(0),
                step,
                version: None,
                operation: INSTALLER_OPERATION_INSTALL,
                method: None,
                log_dir: None,
                apply_run_id: None,
                path: Some(&path),
                sha256: Some(&artifact.sha256),
            })
            .expect("row");
    }

    fn classify(&self, component: &InstallComponent) -> BinaryOwnership {
        classify_component(
            &self.store,
            "duo",
            component,
            self.home.path(),
            self.home.path(),
        )
        .expect("classify")
    }
}

fn harness_of(id: &str) -> InstallComponent {
    components(id)
        .into_iter()
        .find(|component| component.role == ComponentRole::Harness)
        .expect("harness component")
}

#[test]
fn components_follow_the_entry_kind() {
    assert_eq!(
        components("solo"),
        vec![InstallComponent {
            role: ComponentRole::Harness,
            step: STEP_INSTALL,
            command: "ownership-test-solo".to_owned(),
        }]
    );
    assert_eq!(
        components("duo"),
        vec![
            InstallComponent {
                role: ComponentRole::Harness,
                step: STEP_HARNESS,
                command: "ownership-test-duo".to_owned(),
            },
            InstallComponent {
                role: ComponentRole::Adapter,
                step: STEP_ADAPTER,
                command: "ownership-test-duo-acp".to_owned(),
            },
        ]
    );
    // A harness the adapter bundles has no binary of its own to detect.
    assert_eq!(
        components("bundled"),
        vec![InstallComponent {
            role: ComponentRole::Adapter,
            step: STEP_ADAPTER,
            command: "ownership-test-bundled-acp".to_owned(),
        }]
    );
}

#[test]
fn a_missing_binary_is_absent() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.classify(&harness_of("duo")),
        BinaryOwnership::Absent
    );
}

#[test]
fn a_binary_matching_a_ran_row_by_path_and_sha256_is_installed() {
    let fixture = Fixture::new();
    let binary = fixture.bin_dir().join("ownership-test-duo");
    fixture.write_binary(&binary, "installed");
    fixture.record("ran", STEP_HARNESS, &binary);

    assert!(matches!(
        fixture.classify(&harness_of("duo")),
        BinaryOwnership::Installed(artifact) if artifact.path == binary
    ));
}

#[test]
fn a_binary_without_a_matching_row_is_foreign() {
    let fixture = Fixture::new();
    let binary = fixture.bin_dir().join("ownership-test-duo");
    fixture.write_binary(&binary, "installed");
    fixture.record("ran", STEP_HARNESS, &binary);
    // Replaced in place after acp-stack installed it: same path, different bytes.
    fixture.write_binary(&binary, "replaced");

    assert!(matches!(
        fixture.classify(&harness_of("duo")),
        BinaryOwnership::Foreign(artifact) if artifact.path == binary
    ));
}

#[test]
fn a_row_for_another_step_or_a_failed_row_does_not_claim_a_binary() {
    let fixture = Fixture::new();
    let binary = fixture.bin_dir().join("ownership-test-duo");
    fixture.write_binary(&binary, "installed");
    fixture.record("ran", STEP_ADAPTER, &binary);
    fixture.record("failed", STEP_HARNESS, &binary);

    assert!(matches!(
        fixture.classify(&harness_of("duo")),
        BinaryOwnership::Foreign(_)
    ));
}

#[test]
fn a_binary_matching_a_kept_row_is_kept() {
    let fixture = Fixture::new();
    let binary = fixture.bin_dir().join("ownership-test-duo");
    fixture.write_binary(&binary, "vendor");
    fixture.record("kept", STEP_HARNESS, &binary);

    assert!(matches!(
        fixture.classify(&harness_of("duo")),
        BinaryOwnership::Kept(artifact) if artifact.path == binary
    ));
}

#[test]
fn a_link_into_the_managed_bundles_root_is_installed_without_a_row() {
    let fixture = Fixture::new();
    let release_binary = managed_bundles_dir(fixture.home.path())
        .join("ownership-test-duo")
        .join("releases")
        .join("v1.0.0")
        .join("bin")
        .join("ownership-test-duo");
    fixture.write_binary(&release_binary, "bundle");
    let link = fixture.bin_dir().join("ownership-test-duo");
    std::os::unix::fs::symlink(&release_binary, &link).expect("bundle link");

    assert!(matches!(
        fixture.classify(&harness_of("duo")),
        BinaryOwnership::Installed(artifact) if artifact.path == link
    ));
}

#[test]
fn a_link_that_climbs_out_of_the_managed_bundles_root_is_foreign() {
    let fixture = Fixture::new();
    let bundles = managed_bundles_dir(fixture.home.path());
    std::fs::create_dir_all(&bundles).expect("bundles root");
    let outside_binary = bundles
        .parent()
        .expect("bundles parent")
        .join("outside")
        .join("ownership-test-duo");
    fixture.write_binary(&outside_binary, "outside");
    let link = fixture.bin_dir().join("ownership-test-duo");
    let climbing_target = bundles
        .join("..")
        .join("outside")
        .join("ownership-test-duo");
    std::os::unix::fs::symlink(&climbing_target, &link).expect("climbing link");

    assert!(matches!(
        fixture.classify(&harness_of("duo")),
        BinaryOwnership::Foreign(artifact) if artifact.path == link
    ));
}

#[test]
fn an_unreadable_binary_is_an_inspect_failure() {
    // Root reads a mode 000 file regardless, so the read never fails there.
    if crate::ownership::process_euid() == 0 {
        return;
    }
    let fixture = Fixture::new();
    let binary = fixture.bin_dir().join("ownership-test-duo");
    fixture.write_binary(&binary, "unreadable");
    std::fs::set_permissions(&binary, std::os::unix::fs::PermissionsExt::from_mode(0o000))
        .expect("chmod 000");

    let error = classify_component(
        &fixture.store,
        "duo",
        &harness_of("duo"),
        fixture.home.path(),
        fixture.home.path(),
    )
    .expect_err("an unreadable binary cannot be hashed");

    assert!(
        matches!(&error, StackError::AgentBinaryInspect { path, .. } if *path == binary),
        "{error:?}"
    );
}

#[test]
fn an_npm_bin_link_is_traced_through_its_recorded_link_path() {
    let fixture = Fixture::new();
    let package_entry = fixture
        .home
        .path()
        .join(".local/lib/node_modules/@example/duo/dist/cli.js");
    fixture.write_binary(&package_entry, "npm");
    let link = fixture.bin_dir().join("ownership-test-duo");
    std::os::unix::fs::symlink(&package_entry, &link).expect("npm bin link");

    assert!(
        matches!(
            fixture.classify(&harness_of("duo")),
            BinaryOwnership::Foreign(_)
        ),
        "a link outside the bundles root with no row is foreign"
    );

    fixture.record("ran", STEP_HARNESS, &link);
    assert!(matches!(
        fixture.classify(&harness_of("duo")),
        BinaryOwnership::Installed(artifact) if artifact.path == link
    ));
}
