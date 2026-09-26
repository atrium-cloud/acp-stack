//! `acps init` against an agent CLI and ACP adapter that acp-stack did not install: the
//! `--existing-agent` choice, the default replacement, and the adapter that is always replaced.

use crate::common::cli::*;
use acp_stack::state::{InstallerRun, StateStore, default_state_path};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const NATIVE_AGENT: &str = "existing-cli";
const ADAPTER_AGENT: &str = "existing-duo";
const PINNED_AGENT: &str = "pinned-cli";
const PINNED_REPO: &str = "example-owner/pinned-cli";
const PINNED_TAG: &str = "v3.1.0";
const REPINNED_TAG: &str = "v3.1.1";
const LATEST_TAG: &str = "v3.2.0";
const RELEASE_TAGS: [&str; 3] = [PINNED_TAG, REPINNED_TAG, LATEST_TAG];
/// A dependency whose install always fails, so a run stops at deps_apply after agent_install.
const FAILING_DEP: &str = "acpstack-existing-agent-failtool=exit 3";
const CLI_RECIPE_MARKER: &str = "cli-recipe-ran";
const DUO_CLI_RECIPE_MARKER: &str = "duo-cli-recipe-ran";
const ADAPTER_RECIPE_MARKER: &str = "adapter-recipe-ran";
const CONFIG_RELATIVE_PATH: &str = ".config/acp-stack/acps-config.toml";

/// A recipe that writes `name` into `~/.local/bin` as a script printing `version`, then leaves
/// `marker` in the home directory so a test can tell whether the recipe ran.
fn recipe(name: &str, version: &str, marker: &str) -> String {
    format!(
        r#"mkdir -p "$HOME/.local/bin" && printf '#!/bin/sh\necho {name} {version}\n' > "$HOME/.local/bin/{name}" && chmod 755 "$HOME/.local/bin/{name}" && touch "$HOME/{marker}""#
    )
}

fn write_registry(home: &Path) {
    let config_dir = home.join(".config/acp-stack");
    fs::create_dir_all(&config_dir).expect("config dir");
    fs::write(
        config_dir.join("agents.toml"),
        format!(
            r#"
[[agents]]
id = "{NATIVE_AGENT}"
name = "Existing CLI"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/existing-cli.md"

[agents.harness]
id = "{NATIVE_AGENT}"

[agents.harness.install.shell]
script = '''{cli_recipe}'''
creates = "{NATIVE_AGENT}"

[[agents]]
id = "{ADAPTER_AGENT}"
name = "Existing Duo"
kind = "adapter"
headless_compatible = true
support_doc = "docs/agents/existing-duo.md"

[agents.adapter]
id = "existing-duo-acp"

[agents.adapter.install.shell]
script = '''{adapter_recipe}'''
creates = "existing-duo-acp"

[agents.harness]
id = "existing-duo"

[agents.harness.install.shell]
script = '''{duo_cli_recipe}'''
creates = "existing-duo"

[[agents]]
id = "{PINNED_AGENT}"
name = "Pinned CLI"
kind = "native"
headless_compatible = true
support_doc = "docs/agents/pinned-cli.md"
github = "https://github.com/{PINNED_REPO}"

[agents.harness]
id = "{PINNED_AGENT}"

[agents.harness.install.github]
asset_pattern = "{PINNED_AGENT}"
archive = "none"
binary_name = "{PINNED_AGENT}"
"#,
            cli_recipe = recipe(NATIVE_AGENT, "2.0.0", CLI_RECIPE_MARKER),
            adapter_recipe = recipe("existing-duo-acp", "2.0.0", ADAPTER_RECIPE_MARKER),
            duo_cli_recipe = recipe("existing-duo", "2.0.0", DUO_CLI_RECIPE_MARKER),
        ),
    )
    .expect("agent registry");
}

/// The binary `PINNED_AGENT`'s release `tag` ships, distinct per tag.
fn release_bytes(tag: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\necho {PINNED_AGENT} {}\n",
        tag.trim_start_matches('v')
    )
    .into_bytes()
}

/// A GitHub Releases stand-in that serves `RELEASE_TAGS` of `PINNED_REPO` by tag, with
/// `LATEST_TAG` as the latest release, so a test tells which release an install fetched by the
/// binary it left.
fn start_release_server() -> String {
    use axum::extract::{Path as UrlPath, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    fn release_response(
        base_url: &str,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> axum::response::Response {
        if format!("{owner}/{repo}") != PINNED_REPO || !RELEASE_TAGS.contains(&tag) {
            return (StatusCode::NOT_FOUND, "unknown release").into_response();
        }
        axum::Json(serde_json::json!({
            "tag_name": tag,
            "assets": [{
                "name": PINNED_AGENT,
                "browser_download_url": format!("{base_url}/assets/{tag}/{PINNED_AGENT}"),
                "size": release_bytes(tag).len(),
            }],
        }))
        .into_response()
    }

    async fn tagged_release(
        State(base_url): State<String>,
        UrlPath((owner, repo, tag)): UrlPath<(String, String, String)>,
    ) -> axum::response::Response {
        release_response(&base_url, &owner, &repo, &tag)
    }

    async fn latest_release(
        State(base_url): State<String>,
        UrlPath((owner, repo)): UrlPath<(String, String)>,
    ) -> axum::response::Response {
        release_response(&base_url, &owner, &repo, LATEST_TAG)
    }

    async fn asset(UrlPath((tag, _name)): UrlPath<(String, String)>) -> axum::response::Response {
        if !RELEASE_TAGS.contains(&tag.as_str()) {
            return (StatusCode::NOT_FOUND, "unknown asset").into_response();
        }
        (
            [("content-type", "application/octet-stream")],
            release_bytes(&tag),
        )
            .into_response()
    }

    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let base_url = format!("http://{}", listener.local_addr().expect("addr"));
            let router = axum::Router::new()
                .route(
                    "/repos/{owner}/{repo}/releases/tags/{tag}",
                    get(tagged_release),
                )
                .route("/repos/{owner}/{repo}/releases/latest", get(latest_release))
                .route("/assets/{tag}/{name}", get(asset))
                .with_state(base_url.clone());
            sender.send(base_url).expect("send base url");
            axum::serve(listener, router).await.expect("serve");
        });
    });
    receiver.recv().expect("release server base url")
}

/// A binary on the managed PATH that no installer run recorded.
fn write_foreign_binary(home: &Path, name: &str, body: &str) -> PathBuf {
    let path = home.join(".local/bin").join(name);
    fs::create_dir_all(path.parent().expect("bin dir")).expect("bin dir");
    fs::write(&path, body).expect("foreign binary");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn sha256_of(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).expect("read binary")))
}

fn init(home: &Path, agent: &str, extra: &[&str]) -> assert_cmd::Command {
    let workspace = home.join("workspace");
    fs::create_dir_all(workspace.join("uploads")).expect("workspace");
    let mut command = acps_command_without_placebo(home);
    command
        .args(["init", "--agent", agent, "--skip-testflight"])
        .arg("--workspace-root")
        .arg(&workspace)
        .arg("--workspace-uploads")
        .arg(workspace.join("uploads"))
        .args(extra);
    command
}

/// A fresh `acps init` against the existing config without `--agent`, so the registry entry is
/// not re-applied and a configured pin survives into the install step.
fn reinit(home: &Path, extra: &[&str]) -> assert_cmd::Command {
    let mut command = acps_command_without_placebo(home);
    command
        .args(["init", "--non-interactive", "--skip-testflight"])
        .args(extra);
    command
}

fn configured_pin(home: &Path) -> Option<String> {
    acp_stack::config::load_config_from_str(
        &fs::read_to_string(home.join(CONFIG_RELATIVE_PATH)).expect("config"),
    )
    .expect("config validates")
    .agent
    .harness_version
}

fn latest_row(home: &Path, agent: &str, step: &str) -> InstallerRun {
    let store = StateStore::open(default_state_path(home)).expect("state store");
    store
        .query_installer_runs_filtered(Some(agent), 64)
        .expect("installer history")
        .into_iter()
        .find(|row| row.step == step)
        .unwrap_or_else(|| panic!("no `{step}` installer row for {agent}"))
}

#[test]
fn a_foreign_agent_cli_is_replaced_with_its_latest_release_by_default() {
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());
    let foreign = write_foreign_binary(
        home.path(),
        NATIVE_AGENT,
        "#!/bin/sh\necho existing-cli 1.0.0\n",
    );

    init(home.path(), NATIVE_AGENT, &[])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "replacing the agent CLI at {}, which acp-stack did not install, with its latest release",
            foreign.display()
        )));

    assert!(home.path().join(CLI_RECIPE_MARKER).exists());
    let row = latest_row(home.path(), NATIVE_AGENT, "install");
    assert_eq!(row.status, "ran");
    assert_eq!(row.path.as_deref(), Some(foreign.to_str().expect("utf8")));
    assert_eq!(row.sha256, Some(sha256_of(&foreign)));
}

#[test]
fn use_existing_keeps_a_foreign_agent_cli_and_records_it_kept() {
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());
    let body = "#!/bin/sh\necho existing-cli 1.0.0\n";
    let foreign = write_foreign_binary(home.path(), NATIVE_AGENT, body);

    init(
        home.path(),
        NATIVE_AGENT,
        &["--existing-agent", "use-existing"],
    )
    .assert()
    .success()
    .stdout(predicates::str::contains(format!(
        "keeping the agent CLI at {}",
        foreign.display()
    )));

    assert!(
        !home.path().join(CLI_RECIPE_MARKER).exists(),
        "a kept CLI is never reinstalled"
    );
    assert_eq!(fs::read_to_string(&foreign).expect("kept binary"), body);
    let row = latest_row(home.path(), NATIVE_AGENT, "install");
    assert_eq!(row.status, "kept");
    assert_eq!(row.path.as_deref(), Some(foreign.to_str().expect("utf8")));
    assert_eq!(row.sha256, Some(sha256_of(&foreign)));
    assert_eq!(row.version.as_deref(), Some("1.0.0"));
}

#[test]
fn a_foreign_adapter_is_replaced_even_when_the_cli_is_kept() {
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());
    let foreign_cli =
        write_foreign_binary(home.path(), "existing-duo", "#!/bin/sh\necho duo 1.0.0\n");
    let foreign_adapter = write_foreign_binary(
        home.path(),
        "existing-duo-acp",
        "#!/bin/sh\necho duo-acp 1.0.0\n",
    );

    init(
        home.path(),
        ADAPTER_AGENT,
        &["--existing-agent", "use-existing"],
    )
    .assert()
    .success()
    .stdout(predicates::str::contains(format!(
        "replacing the ACP adapter at {}, which acp-stack did not install, with its latest release",
        foreign_adapter.display()
    )));

    assert!(!home.path().join(DUO_CLI_RECIPE_MARKER).exists());
    assert!(home.path().join(ADAPTER_RECIPE_MARKER).exists());
    let harness = latest_row(home.path(), ADAPTER_AGENT, "harness");
    assert_eq!(harness.status, "kept");
    assert_eq!(
        harness.path.as_deref(),
        Some(foreign_cli.to_str().expect("utf8"))
    );
    let adapter = latest_row(home.path(), ADAPTER_AGENT, "adapter");
    assert_eq!(adapter.status, "ran");
    assert_eq!(adapter.sha256, Some(sha256_of(&foreign_adapter)));
}

#[test]
fn an_agent_version_replaces_a_foreign_cli_with_that_release_and_keeps_the_pin() {
    let release_server = start_release_server();
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());
    let foreign = write_foreign_binary(
        home.path(),
        PINNED_AGENT,
        "#!/bin/sh\necho pinned-cli 1.0.0\n",
    );

    init(home.path(), PINNED_AGENT, &["--agent-version", PINNED_TAG])
        .env("ACP_STACK_GITHUB_API_BASE", &release_server)
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "replacing the agent CLI at {}, which acp-stack did not install, with version {PINNED_TAG}",
            foreign.display()
        )));

    assert_eq!(configured_pin(home.path()).as_deref(), Some(PINNED_TAG));
    assert_eq!(
        fs::read(&foreign).expect("installed binary"),
        release_bytes(PINNED_TAG)
    );
    let row = latest_row(home.path(), PINNED_AGENT, "install");
    assert_eq!(row.status, "ran");
    assert_eq!(row.version.as_deref(), Some(PINNED_TAG));
    assert_eq!(row.path.as_deref(), Some(foreign.to_str().expect("utf8")));
    assert_eq!(row.sha256, Some(sha256_of(&foreign)));
}

/// Install `PINNED_TAG` of `PINNED_AGENT`, then rewrite the binary outside acp-stack so it no
/// longer matches the row that installed it.
fn pinned_install_made_foreign(home: &Path, release_server: &str) -> PathBuf {
    init(home, PINNED_AGENT, &["--agent-version", PINNED_TAG])
        .env("ACP_STACK_GITHUB_API_BASE", release_server)
        .assert()
        .success();
    let binary = home.join(".local/bin").join(PINNED_AGENT);
    fs::write(&binary, "#!/bin/sh\necho pinned-cli 0.9.0\n").expect("rewrite binary");
    binary
}

#[test]
fn a_configured_pin_replaces_a_foreign_cli_with_that_version_by_default() {
    let release_server = start_release_server();
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());
    let foreign = pinned_install_made_foreign(home.path(), &release_server);

    reinit(home.path(), &[])
        .env("ACP_STACK_GITHUB_API_BASE", &release_server)
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "replacing the agent CLI at {}, which acp-stack did not install, with version {PINNED_TAG}",
            foreign.display()
        )));

    assert_eq!(configured_pin(home.path()).as_deref(), Some(PINNED_TAG));
    assert_eq!(
        fs::read(&foreign).expect("installed binary"),
        release_bytes(PINNED_TAG)
    );
    let row = latest_row(home.path(), PINNED_AGENT, "install");
    assert_eq!(row.version.as_deref(), Some(PINNED_TAG));
    assert_eq!(row.sha256, Some(sha256_of(&foreign)));
}

#[test]
fn replace_latest_clears_a_configured_pin_and_installs_the_latest_release() {
    let release_server = start_release_server();
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());
    let foreign = pinned_install_made_foreign(home.path(), &release_server);

    reinit(home.path(), &["--existing-agent", "replace-latest"])
        .env("ACP_STACK_GITHUB_API_BASE", &release_server)
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "replacing the agent CLI at {}, which acp-stack did not install, with its latest release",
            foreign.display()
        )));

    assert_eq!(configured_pin(home.path()), None);
    assert_eq!(
        fs::read(&foreign).expect("installed binary"),
        release_bytes(LATEST_TAG)
    );
    let row = latest_row(home.path(), PINNED_AGENT, "install");
    assert_eq!(row.version.as_deref(), Some(LATEST_TAG));
    assert_eq!(row.sha256, Some(sha256_of(&foreign)));
}

#[test]
fn a_resume_with_a_changed_agent_version_installs_the_new_pin() {
    let release_server = start_release_server();
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());

    // No binary predates the run, so only the pin can keep the resume from skipping the install.
    init(
        home.path(),
        PINNED_AGENT,
        &[
            "--agent-version",
            PINNED_TAG,
            "--dep",
            FAILING_DEP,
            "--deps-apply",
            "--deps-apply-yes",
        ],
    )
    .env("ACP_STACK_GITHUB_API_BASE", &release_server)
    .assert()
    .failure()
    .stderr(predicates::str::contains(
        "dependency apply produced failing actions",
    ));
    let binary = home.path().join(".local/bin").join(PINNED_AGENT);
    assert_eq!(
        fs::read(&binary).expect("installed binary"),
        release_bytes(PINNED_TAG)
    );

    // The dependency still fails, so the resume stops at deps_apply again.
    init(
        home.path(),
        PINNED_AGENT,
        &["--resume", "--agent-version", REPINNED_TAG],
    )
    .env("ACP_STACK_GITHUB_API_BASE", &release_server)
    .assert()
    .failure()
    .stderr(predicates::str::contains(
        "dependency apply produced failing actions",
    ));

    assert_eq!(configured_pin(home.path()).as_deref(), Some(REPINNED_TAG));
    assert_eq!(
        fs::read(&binary).expect("reinstalled binary"),
        release_bytes(REPINNED_TAG)
    );
    assert_eq!(
        latest_row(home.path(), PINNED_AGENT, "install")
            .version
            .as_deref(),
        Some(REPINNED_TAG)
    );
    let store = StateStore::open(default_state_path(home.path())).expect("state store");
    let run = store
        .latest_init_run()
        .expect("init runs")
        .expect("the resumed run");
    let install_step = store
        .query_init_steps(&run.id)
        .expect("init steps")
        .into_iter()
        .find(|step| step.kind == "agent_install")
        .expect("agent_install step");
    assert_eq!(install_step.status, "succeeded");
}

#[test]
fn an_agent_version_for_a_script_only_cli_is_refused_before_config_is_written() {
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());

    init(home.path(), NATIVE_AGENT, &["--agent-version", "1.2.3"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "agent `existing-cli` cannot install version `1.2.3` of its CLI: its CLI installs only through the vendor's install script",
        ));
    assert!(!home.path().join(CLI_RECIPE_MARKER).exists());
    assert!(
        !home.path().join(CONFIG_RELATIVE_PATH).exists(),
        "a refused pin leaves no starter config behind"
    );
}

#[test]
fn existing_agent_flags_that_no_install_honors_are_refused_up_front() {
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());

    init(
        home.path(),
        NATIVE_AGENT,
        &["--existing-agent", "replace-version"],
    )
    .assert()
    .failure()
    .stderr(predicates::str::contains(
        "`replace-version` requires --agent-version",
    ));
    init(
        home.path(),
        NATIVE_AGENT,
        &[
            "--existing-agent",
            "use-existing",
            "--agent-version",
            "1.2.3",
        ],
    )
    .assert()
    .failure()
    .stderr(predicates::str::contains(
        "`use-existing` conflicts with --agent-version",
    ));
    assert!(!home.path().join(CLI_RECIPE_MARKER).exists());
}

#[test]
fn a_resumed_install_keeps_following_the_recorded_choice() {
    let home = tempfile::tempdir().expect("home");
    write_registry(home.path());
    // Not runnable, so the kept CLI fails its spawn gate and the run stops at agent_install.
    let foreign = write_foreign_binary(home.path(), NATIVE_AGENT, "not an executable");

    init(
        home.path(),
        NATIVE_AGENT,
        &["--existing-agent", "use-existing"],
    )
    .assert()
    .failure();

    fs::write(&foreign, "#!/bin/sh\necho existing-cli 1.0.0\n").expect("repair binary");
    // A bare resume replays the recorded use-existing; the default would run the recipe.
    init(home.path(), NATIVE_AGENT, &["--resume"])
        .assert()
        .success();

    assert!(!home.path().join(CLI_RECIPE_MARKER).exists());
    assert_eq!(
        latest_row(home.path(), NATIVE_AGENT, "install").status,
        "kept"
    );
}
