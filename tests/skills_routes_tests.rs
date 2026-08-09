#![cfg(feature = "test-fixtures")]

//! End-to-end coverage for the day-2 Agent Skills routes: the session-tier reads
//! `GET /v1/agent/skills`, `GET /v1/agent/skills/catalog`,
//! `GET /v1/agent/skills/source`, and the admin-tier mutations
//! `POST /v1/agent/skills/add` / `POST /v1/agent/skills/remove` and
//! `POST /v1/agent/skills/sources/add` / `POST /v1/agent/skills/sources/remove`.
//!
//! Reads, config-source persistence, and validation/auth failures are covered
//! here; the live fetch paths (`add`'s install and `source get`'s download +
//! frontmatter parse) hit GitHub, so those are left to manual/e2e verification.

use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

mod common;
use common::HomeEnvGuard;
use common::agent::{
    AgentHarness, admin_bearer, http, session_bearer, test_config, write_installed_skill,
};

/// Registry override that keeps the default `opencode` agent skills-capable but
/// adds a harness link dir, so removal exercises the symlink-mirror prune.
fn write_opencode_linked_skills_override(config_dir: &std::path::Path) {
    let body = r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_model = true
set_mode = true
supports_agent_skills = true
agent_skills_install_dir = "~/.agents/skills"
agent_skills_link_dir = "~/.claude/skills"
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "true"

[agents.harness.install.shell]
script = "true"
creates = "true"
"#;
    std::fs::write(config_dir.join("agents.toml"), body).expect("registry override");
}

/// Registry override that strips skills support from the default agent.
fn write_opencode_no_skills_override(config_dir: &std::path::Path) {
    let body = r#"
[[agents]]
id = "opencode"
name = "OpenCode"
kind = "native"
headless_compatible = true
set_model = true
set_mode = true
support_doc = "docs/agents/opencode.md"

[agents.harness]
id = "true"

[agents.harness.install.shell]
script = "true"
creates = "true"
"#;
    std::fs::write(config_dir.join("agents.toml"), body).expect("registry override");
}

#[tokio::test]
async fn skills_add_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/add", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&serde_json::json!({ "source": "anthropic", "skills": ["docx"] }))
        .send()
        .await
        .expect("send add");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn skills_remove_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/remove", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&serde_json::json!({ "skill": "docx" }))
        .send()
        .await
        .expect("send remove");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn skills_list_returns_installed_skills_sorted() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let install_root = tempdir.path().join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    write_installed_skill(&install_root, "code-review", "# Code Review\n");

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .get(format!("{}/v1/agent/skills", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send list");
    let status = response.status();
    let body: Value = response.json().await.expect("list json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["agent_id"], "opencode");
    assert_eq!(body["data"]["supported"], true);
    let names: Vec<&str> = body["data"]["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .filter_map(|skill| skill["name"].as_str())
        .collect();
    assert_eq!(names, ["code-review", "repo-map"]);
    // Provenance: the source id recorded in the managed marker at install time
    // is surfaced per skill.
    for skill in body["data"]["skills"].as_array().expect("skills array") {
        assert_eq!(skill["source"], "test-source", "skill: {skill}");
    }
}

#[tokio::test]
async fn skills_list_omits_source_for_unmanaged_skill() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let skill_dir = tempdir.path().join(".agents/skills/hand-made");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join("SKILL.md"), "# Mine\n").expect("descriptor");

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .get(format!("{}/v1/agent/skills", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send list");
    let status = response.status();
    let body: Value = response.json().await.expect("list json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let skill = &body["data"]["skills"].as_array().expect("skills array")[0];
    assert_eq!(skill["name"], "hand-made");
    assert!(skill["source"].is_null(), "skill: {skill}");
}

#[tokio::test]
async fn skills_list_rejects_admin_key() {
    // Strict tiering also holds in reverse: admin keys must not work on
    // session-tier reads.
    let harness = AgentHarness::spawn().await;
    for path in ["/v1/agent/skills", "/v1/agent/skills/catalog"] {
        let response = http()
            .await
            .get(format!("{}{path}", harness.base_url))
            .header("Authorization", admin_bearer())
            .send()
            .await
            .expect("send read");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "path: {path}");
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["error"]["code"], "auth.wrong_kind", "path: {path}");
    }
}

#[tokio::test]
async fn skills_list_reports_unsupported_agent() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_opencode_no_skills_override(&config_dir);

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .get(format!("{}/v1/agent/skills", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send list");
    let status = response.status();
    let body: Value = response.json().await.expect("list json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["supported"], false);
    assert!(body["data"]["install_dir"].is_null());
    assert!(
        body["data"]["skills"]
            .as_array()
            .expect("skills array")
            .is_empty()
    );
}

#[tokio::test]
async fn skills_catalog_lists_builtin_sources() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .get(format!("{}/v1/agent/skills/catalog", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send catalog");
    let status = response.status();
    let body: Value = response.json().await.expect("catalog json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let sources = body["data"]["sources"].as_array().expect("sources array");
    assert!(!sources.is_empty());
    let anthropic = sources
        .iter()
        .find(|source| source["alias"] == "anthropic")
        .expect("anthropic source present");
    let essential: Vec<&str> = anthropic["essential"]
        .as_array()
        .expect("essential array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(essential.contains(&"docx"), "essential: {essential:?}");
}

#[tokio::test]
async fn skills_add_rejects_unknown_source() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/add", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "source": "not-a-real-source", "skills": ["x"] }))
        .send()
        .await
        .expect("send add");
    let status = response.status();
    let body: Value = response.json().await.expect("add json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.skill_install_invalid_source");
}

#[tokio::test]
async fn skills_add_rejects_empty_skills() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/add", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "source": "anthropic", "skills": [] }))
        .send()
        .await
        .expect("send add");
    let status = response.status();
    let body: Value = response.json().await.expect("add json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "config.invalid");
}

#[tokio::test]
async fn skills_add_rejects_unsupported_agent() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_opencode_no_skills_override(&config_dir);

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/add", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "source": "anthropic", "skills": ["docx"] }))
        .send()
        .await
        .expect("send add");
    let status = response.status();
    let body: Value = response.json().await.expect("add json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn skills_remove_rejects_unsupported_agent() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_opencode_no_skills_override(&config_dir);

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/remove", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "skill": "docx" }))
        .send()
        .await
        .expect("send remove");
    let status = response.status();
    let body: Value = response.json().await.expect("remove json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn skills_remove_rejects_malformed_skill_name() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/remove", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "skill": "../evil" }))
        .send()
        .await
        .expect("send remove");
    let status = response.status();
    let body: Value = response.json().await.expect("remove json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn skills_remove_missing_skill_is_not_found() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    std::fs::create_dir_all(tempdir.path().join(".agents/skills")).expect("install root");

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/remove", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "skill": "missing" }))
        .send()
        .await
        .expect("send remove");
    let status = response.status();
    let body: Value = response.json().await.expect("remove json");

    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.skill_not_installed");
}

#[tokio::test]
async fn skills_remove_refuses_skill_not_installed_by_acp_stack() {
    // A folder placed in the install root by hand has a regular SKILL.md but
    // no managed marker: removal must refuse it and leave it in place.
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let skill_dir = tempdir.path().join(".agents/skills/my-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join("SKILL.md"), "# Mine\n").expect("descriptor");

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/remove", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "skill": "my-skill" }))
        .send()
        .await
        .expect("send remove");
    let status = response.status();
    let body: Value = response.json().await.expect("remove json");

    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.skill_install_target_conflict");
    assert!(skill_dir.join("SKILL.md").is_file());
}

#[tokio::test]
async fn skills_source_add_requires_admin_key() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/sources/add", harness.base_url))
        .header("Authorization", session_bearer())
        .json(&serde_json::json!({ "alias": "my-org", "github": "my-org/skills" }))
        .send()
        .await
        .expect("send add");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "auth.wrong_kind");
}

#[tokio::test]
async fn skills_source_add_persists_and_appears_in_catalog() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let add = client
        .post(format!("{}/v1/agent/skills/sources/add", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({
            "alias": "my-org",
            "github": "my-org/skills",
            "branch": "dev",
            "trusted": true
        }))
        .send()
        .await
        .expect("send add");
    let status = add.status();
    let body: Value = add.json().await.expect("add json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["alias"], "my-org");
    assert_eq!(body["data"]["branch"], "dev");
    assert_eq!(body["data"]["sources"], 1);

    // Persisted to config on disk.
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(written.contains("[[skills.sources]]"), "config: {written}");
    assert!(written.contains(r#"alias = "my-org""#), "config: {written}");

    // Surfaced in the catalog listing as a user (non-catalog) source.
    let catalog: Value = client
        .get(format!("{}/v1/agent/skills/catalog", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send catalog")
        .json()
        .await
        .expect("catalog json");
    let sources = catalog["data"]["sources"]
        .as_array()
        .expect("sources array");
    let mine = sources
        .iter()
        .find(|source| source["alias"] == "my-org")
        .expect("user source present in catalog");
    assert_eq!(mine["catalog"], false);
    assert_eq!(mine["trusted"], true);
    assert_eq!(mine["repo"], "my-org/skills");
}

#[tokio::test]
async fn skills_source_add_rejects_catalog_alias() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/sources/add", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "alias": "anthropic", "github": "x/skills" }))
        .send()
        .await
        .expect("send add");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn skills_source_add_rejects_duplicate_alias() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    for github in ["my-org/skills", "other-org/skills"] {
        let response = client
            .post(format!("{}/v1/agent/skills/sources/add", harness.base_url))
            .header("Authorization", admin_bearer())
            .json(&serde_json::json!({ "alias": "my-org", "github": github }))
            .send()
            .await
            .expect("send add");
        let status = response.status();
        let body: Value = response.json().await.expect("json");
        if github == "my-org/skills" {
            assert_eq!(status, StatusCode::OK, "body: {body}");
        } else {
            assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
            assert_eq!(body["error"]["code"], "request.invalid_param");
        }
    }
}

#[tokio::test]
async fn skills_source_add_rejects_malformed_github() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/sources/add", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "alias": "my-org", "github": "not-a-repo" }))
        .send()
        .await
        .expect("send add");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "request.invalid_param");
}

#[tokio::test]
async fn skills_source_remove_persists_and_404_when_absent() {
    let harness = AgentHarness::spawn().await;
    let client = http().await;

    let absent = client
        .post(format!(
            "{}/v1/agent/skills/sources/remove",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "alias": "ghost" }))
        .send()
        .await
        .expect("send remove");
    let absent_status = absent.status();
    let absent_body: Value = absent.json().await.expect("json");
    assert_eq!(absent_status, StatusCode::NOT_FOUND, "body: {absent_body}");
    assert_eq!(
        absent_body["error"]["code"],
        "agent.skill_source_not_configured"
    );

    client
        .post(format!("{}/v1/agent/skills/sources/add", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "alias": "my-org", "github": "my-org/skills" }))
        .send()
        .await
        .expect("send add");
    let remove = client
        .post(format!(
            "{}/v1/agent/skills/sources/remove",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "alias": "my-org" }))
        .send()
        .await
        .expect("send remove");
    let status = remove.status();
    let body: Value = remove.json().await.expect("json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["sources"], 0);
    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(
        !written.contains(r#"alias = "my-org""#),
        "config: {written}"
    );
}

#[tokio::test]
async fn skills_surface_survives_invalid_source_entry_and_remove_heals_it() {
    // A hand-edited invalid `[[skills.sources]]` entry must not brick the
    // skills surface: reads drop it like daemon boot does, and a sources
    // mutation both succeeds and heals the bad entry out of the file.
    let harness = AgentHarness::spawn().await;
    let client = http().await;
    let mut config = std::fs::read_to_string(&harness.config_path).expect("read config");
    config.push_str(concat!(
        "\n[[skills.sources]]\nalias = \"my-org\"\ngithub = \"my-org/skills\"\n",
        "\n[[skills.sources]]\nalias = \"Bad_Alias\"\ngithub = \"a/skills\"\n",
    ));
    std::fs::write(&harness.config_path, config).expect("write config");

    let catalog: Value = client
        .get(format!("{}/v1/agent/skills/catalog", harness.base_url))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send catalog")
        .json()
        .await
        .expect("catalog json");
    let aliases: Vec<&str> = catalog["data"]["sources"]
        .as_array()
        .expect("sources array")
        .iter()
        .filter(|source| source["catalog"] == false)
        .filter_map(|source| source["alias"].as_str())
        .collect();
    assert_eq!(aliases, ["my-org"], "catalog: {catalog}");

    let remove = client
        .post(format!(
            "{}/v1/agent/skills/sources/remove",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "alias": "my-org" }))
        .send()
        .await
        .expect("send remove");
    let status = remove.status();
    let body: Value = remove.json().await.expect("remove json");
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let written = std::fs::read_to_string(&harness.config_path).expect("read config");
    assert!(!written.contains("my-org"), "config: {written}");
    assert!(!written.contains("Bad_Alias"), "config: {written}");
}

#[tokio::test]
async fn skills_source_get_rejects_unknown_source() {
    let harness = AgentHarness::spawn().await;
    let response = http()
        .await
        .get(format!(
            "{}/v1/agent/skills/source?source=nonsense",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send get");
    let status = response.status();
    let body: Value = response.json().await.expect("json");

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "agent.skill_install_invalid_source");
}

#[tokio::test]
async fn skills_remove_uninstalls_and_prunes_link() {
    let tempdir = TempDir::new().expect("tempdir");
    let _home = HomeEnvGuard::set(tempdir.path());
    let config_dir = tempdir.path().join(".config/acp-stack");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    write_opencode_linked_skills_override(&config_dir);

    // Canonicalize so the pre-created symlink target matches the canonicalized
    // install root the link refresh resolves (macOS `/var` -> `/private/var`).
    let home = tempdir.path().canonicalize().expect("canonical home");
    let install_root = home.join(".agents/skills");
    write_installed_skill(&install_root, "repo-map", "# Repo Map\n");
    let link_root = home.join(".claude/skills");
    std::fs::create_dir_all(&link_root).expect("link root");
    std::os::unix::fs::symlink(install_root.join("repo-map"), link_root.join("repo-map"))
        .expect("pre-existing mirror link");

    let harness = AgentHarness::spawn_with_config(test_config()).await;
    let response = http()
        .await
        .post(format!("{}/v1/agent/skills/remove", harness.base_url))
        .header("Authorization", admin_bearer())
        .json(&serde_json::json!({ "skill": "repo-map" }))
        .send()
        .await
        .expect("send remove");
    let status = response.status();
    let body: Value = response.json().await.expect("remove json");

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["remove"]["removed"]["name"], "repo-map");
    assert_eq!(body["data"]["skills_link"]["pruned"][0]["name"], "repo-map");
    assert!(!install_root.join("repo-map").exists());
    assert!(std::fs::symlink_metadata(link_root.join("repo-map")).is_err());

    // The removal is recorded in the runtime event log for audit.
    let events: Value = http()
        .await
        .get(format!(
            "{}/v1/logs/events?kind=skill.remove&limit=10",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send events")
        .json()
        .await
        .expect("events json");
    let events = events["data"]["events"].as_array().expect("events array");
    assert!(
        events.iter().any(|event| event["payload_json"]
            .as_str()
            .is_some_and(|payload| payload.contains("repo-map"))),
        "events: {events:?}"
    );
}
