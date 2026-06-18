#[cfg(not(feature = "dev-tools"))]
use assert_cmd::Command;
#[cfg(not(feature = "dev-tools"))]
use predicates::prelude::*;

#[cfg(not(feature = "dev-tools"))]
#[test]
fn production_help_hides_dev_command() {
    let mut cmd = Command::cargo_bin("acps").expect("acps binary");
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(" dev ").not())
        .stdout(predicate::str::contains("Run development-only workflows").not());
}

#[cfg(not(feature = "dev-tools"))]
#[test]
fn production_dev_command_is_unknown() {
    let mut cmd = Command::cargo_bin("acps").expect("acps binary");
    cmd.arg("dev")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand 'dev'"));
}

#[cfg(not(feature = "test-fixtures"))]
#[test]
fn production_build_does_not_expose_placebo_binary() {
    let manifest = std::fs::read_to_string("Cargo.toml").expect("read manifest");
    let value: toml::Value = toml::from_str(&manifest).expect("parse manifest");
    let bins = value
        .get("bin")
        .and_then(toml::Value::as_array)
        .expect("bin array");
    let placebo = bins
        .iter()
        .find(|bin| bin.get("name").and_then(toml::Value::as_str) == Some("placebo-agent"))
        .expect("placebo binary target");
    assert_eq!(
        placebo.get("required-features"),
        Some(&toml::Value::Array(vec![toml::Value::String(
            "test-fixtures".to_owned()
        )]))
    );
}

#[test]
fn default_release_build_keeps_stack_self_update_enabled() {
    let manifest = std::fs::read_to_string("Cargo.toml").expect("read manifest");
    let value: toml::Value = toml::from_str(&manifest).expect("parse manifest");
    let features = value
        .get("features")
        .and_then(toml::Value::as_table)
        .expect("features table");
    let defaults = features
        .get("default")
        .and_then(toml::Value::as_array)
        .expect("default feature array");
    assert!(
        features.contains_key("stack-self-update"),
        "stack-self-update feature must exist so dev builds can omit it explicitly"
    );
    assert!(
        defaults
            .iter()
            .any(|feature| feature.as_str() == Some("stack-self-update")),
        "default release builds must include stack-self-update"
    );

    let release_workflow =
        std::fs::read_to_string(".github/workflows/release.yml").expect("read release workflow");
    assert!(
        release_workflow.contains("bash scripts/build-release.sh"),
        "release workflow must use the release packaging script"
    );
    assert!(
        !release_workflow.contains("--no-default-features"),
        "public release builds must keep default features enabled"
    );
}

#[test]
fn dev_build_workflow_uploads_release_shaped_artifacts_without_self_update() {
    let workflow = std::fs::read_to_string(".github/workflows/dev-build.yml")
        .expect("read dev build workflow");
    assert!(
        workflow.contains("workflow_dispatch:"),
        "dev build workflow must be manual-only"
    );
    assert!(
        !workflow.contains("pull_request:") && !workflow.contains("push:"),
        "dev build workflow must not run on PRs or pushes"
    );
    assert!(
        workflow.contains("permissions:") && workflow.contains("contents: read"),
        "dev build workflow must request read-only contents permission"
    );
    assert!(
        !workflow.contains("contents: write"),
        "dev build workflow must not request release publishing permission"
    );
    assert!(
        workflow.contains("bash scripts/build-release.sh --no-default-features"),
        "dev build workflow must compile without default features"
    );
    assert!(
        workflow.contains("uses: actions/upload-artifact@v4") && workflow.contains("path: dist/"),
        "dev build workflow must upload the full dist directory"
    );
    assert!(
        !workflow.contains("softprops/action-gh-release"),
        "dev build workflow must not publish GitHub Releases"
    );
}

#[test]
fn docker_runtime_includes_registry_install_tools() {
    let dockerfile = std::fs::read_to_string("Dockerfile").expect("read Dockerfile");
    let install_line = dockerfile
        .lines()
        .find(|line| line.contains("apt-get install"))
        .expect("runtime apt install line");
    for tool in ["bash", "curl", "npm"] {
        assert!(
            install_line.contains(tool),
            "Docker runtime must include {tool} for registry install paths"
        );
    }
}

#[test]
fn docker_test_runtime_uses_fixture_enabled_binaries() {
    let dockerfile = std::fs::read_to_string("Dockerfile").expect("read Dockerfile");
    assert!(
        dockerfile.contains(
            "cargo build --locked --release --features test-fixtures --bin acps --bin placebo-agent"
        ),
        "test-runtime must build fixture-enabled runtime binaries for placebo registry support"
    );
    for binary in ["acps", "placebo-agent"] {
        assert!(
            dockerfile.contains(&format!(
                "COPY --from=builder-test /app/target/release/{binary} /usr/local/bin/{binary}"
            )),
            "test-runtime must copy fixture-enabled {binary}"
        );
    }
}

#[test]
fn docker_entrypoint_maps_provider_init_env_vars() {
    let entrypoint =
        std::fs::read_to_string("scripts/docker-entrypoint.sh").expect("read Docker entrypoint");
    for (env_var, flag) in [
        ("ACP_STACK_INIT_PROVIDER", "--provider"),
        ("ACP_STACK_INIT_API_KEY_REF", "--api-key-ref"),
        ("ACP_STACK_INIT_MODEL", "--model"),
        ("ACP_STACK_INIT_WORKSPACE_ROOT", "--workspace-root"),
        ("ACP_STACK_INIT_WORKSPACE_UPLOADS", "--workspace-uploads"),
    ] {
        assert!(
            entrypoint.contains(env_var) && entrypoint.contains(flag),
            "entrypoint must map {env_var} to {flag}"
        );
    }
}

#[test]
fn systemd_installer_includes_registry_install_tools() {
    let installer =
        std::fs::read_to_string("scripts/install-systemd.sh").expect("read systemd installer");
    for tool in ["ca-certificates", "bash", "curl", "npm"] {
        assert!(
            installer.contains(tool),
            "systemd installer must include {tool} for registry install paths"
        );
    }
    assert!(
        installer.contains("missing required OS tools"),
        "systemd installer must fail clearly when registry tools cannot be installed"
    );
}

#[test]
fn railway_docs_require_persistent_workspace_volume() {
    let docs = std::fs::read_to_string("docs/deploy/docker.md").expect("read Docker docs");
    for required in [
        "/home/acp/workspace",
        "ACP_STACK_INIT_WORKSPACE_ROOT",
        "ACP_STACK_INIT_WORKSPACE_UPLOADS",
    ] {
        assert!(
            docs.contains(required),
            "Railway docs must mention {required}"
        );
    }
}

#[test]
fn release_workflow_runs_acceptance_gate() {
    let workflow = std::fs::read_to_string(".github/workflows/release-gate-tests.yml")
        .expect("read release gate workflow");
    assert!(
        workflow.contains("tests/release_acceptance_tests.rs"),
        "release workflow must trigger on release acceptance test changes"
    );
    assert!(
        workflow.contains(
            "cargo test --test release_acceptance_tests --features dev-tools,test-fixtures --locked"
        ),
        "release workflow must run release_acceptance_tests with release fixtures"
    );
    assert!(
        workflow.contains("cargo check --no-default-features --bin acps --locked"),
        "release workflow must compile the dev-build feature set"
    );
}
