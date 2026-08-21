//! Command execution coverage: submit/exit status recording, auto-mode
//! shell-composition escalation, env allowlisting, cwd containment, cancel,
//! timeout, output truncation/replay, progress events, and the WebSocket
//! command/logs topics.

mod common;

use std::time::Duration;

use acp_stack::config::{CommandsConfig, PermissionsConfig};
use common::commands::{
    Harness, HarnessOverrides, admin_auth, approve_pending_command, auth, collect_until, open_ws,
    pending_permission_for_command, session_client, submit, wait_for_terminal,
};
use reqwest::StatusCode;
use serde_json::Value;

#[tokio::test]
async fn submit_runs_command_and_records_exit_status() {
    let harness = Harness::spawn().await;
    let response = submit(&harness, serde_json::json!({"command": "echo hello"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().expect("id").to_owned();

    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["exit_status"], 0);
    assert!(final_body["data"]["duration_ms"].as_i64().unwrap() >= 0);
}

#[tokio::test]
async fn submit_records_failure_status_for_nonzero_exit() {
    let harness = Harness::spawn().await;
    let response = submit(&harness, serde_json::json!({"command": "exit 7"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "failed");
    assert_eq!(final_body["data"]["exit_status"], 7);
}

#[tokio::test]
async fn review_pattern_allowed_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec!["echo *".to_owned()],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "echo flagged"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
}

#[tokio::test]
async fn composed_command_requires_permission_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "echo one && echo two"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(pending.status(), StatusCode::OK);
    let pending_body: Value = pending.json().await.expect("json");
    let permissions_list = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array");
    let entry = permissions_list
        .iter()
        .find(|permission| permission["subject_id"].as_str() == Some(&cmd_id))
        .expect("pending permission row for composed command");
    assert_eq!(entry["detail"]["policy_decision"], "shell-composition");
}

#[tokio::test]
async fn constructed_command_word_requires_permission_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "e''cho one"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let command_id = body["data"]["id"].as_str().expect("command id");
    let permission = pending_permission_for_command(&harness, command_id).await;
    assert_eq!(permission["detail"]["policy_decision"], "shell-composition");
}

#[tokio::test]
async fn parameter_expanded_command_word_requires_permission_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec!["rm *".to_owned()],
        ..PermissionsConfig::default()
    };
    let commands = CommandsConfig {
        default_timeout: "10m".to_owned(),
        cancel_grace: "5s".to_owned(),
        env_allowlist: vec!["X".to_owned()],
        max_output_bytes: 1_048_576,
        ..CommandsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: Some(commands),
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": r"r${X} -rf target", "env": { "X": "m" }}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let command_id = body["data"]["id"].as_str().expect("command id");
    let permission = pending_permission_for_command(&harness, command_id).await;
    assert_eq!(permission["detail"]["policy_decision"], "shell-composition");
}

#[tokio::test]
async fn brace_expanded_command_word_requires_permission_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec!["rm *".to_owned()],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "r{m,} -rf target"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let command_id = body["data"]["id"].as_str().expect("command id");
    let permission = pending_permission_for_command(&harness, command_id).await;
    assert_eq!(permission["detail"]["policy_decision"], "shell-composition");
}

#[tokio::test]
async fn command_substitution_requires_permission_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "echo $(date)"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(pending.status(), StatusCode::OK);
    let pending_body: Value = pending.json().await.expect("json");
    let permissions_list = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array");
    let entry = permissions_list
        .iter()
        .find(|permission| permission["subject_id"].as_str() == Some(&cmd_id))
        .expect("pending permission row for command substitution");
    assert_eq!(entry["detail"]["policy_decision"], "shell-composition");
}

#[tokio::test]
async fn process_substitution_requires_permission_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "cat <(date)"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    assert_eq!(pending.status(), StatusCode::OK);
    let pending_body: Value = pending.json().await.expect("json");
    let permissions_list = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array");
    let entry = permissions_list
        .iter()
        .find(|permission| permission["subject_id"].as_str() == Some(&cmd_id))
        .expect("pending permission row for process substitution");
    assert_eq!(entry["detail"]["policy_decision"], "shell-composition");
}

#[tokio::test]
async fn double_quoted_process_substitution_text_runs_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "echo \"<(date)\""})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["exit_status"], 0);
}

#[tokio::test]
async fn quoted_denied_word_argument_runs_in_auto_mode() {
    let permissions = PermissionsConfig {
        mode: "auto".to_owned(),
        review: vec![],
        deny: vec!["rm *".to_owned()],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": r#"echo "rm -rf target""#}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
}

#[tokio::test]
async fn env_not_on_allowlist_rejected() {
    let commands = CommandsConfig {
        default_timeout: "10m".to_owned(),
        cancel_grace: "5s".to_owned(),
        env_allowlist: vec!["FOO".to_owned()],
        max_output_bytes: 1_048_576,
        ..CommandsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "echo $BAR", "env": {"BAR": "x"}}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.env_not_allowed");
}

#[tokio::test]
async fn env_on_allowlist_reaches_child() {
    let commands = CommandsConfig {
        default_timeout: "10m".to_owned(),
        cancel_grace: "5s".to_owned(),
        env_allowlist: vec!["GREETING".to_owned()],
        max_output_bytes: 1_048_576,
        ..CommandsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "printf %s \"$GREETING\"", "env": {"GREETING": "hi"}}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
}

#[tokio::test]
async fn cwd_outside_workspace_rejected() {
    let harness = Harness::spawn().await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "echo", "cwd": "/etc"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.cwd_outside_workspace");
}

#[tokio::test]
async fn cwd_relative_under_workspace_accepted() {
    let harness = Harness::spawn().await;
    std::fs::create_dir(harness.workspace_root.join("inner")).expect("inner dir");
    let response = submit(
        &harness,
        serde_json::json!({"command": "pwd", "cwd": "inner"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
}

#[cfg(unix)]
#[tokio::test]
async fn cwd_symlink_replacement_before_approval_fails_spawn() {
    let permissions = PermissionsConfig {
        mode: "locked".to_owned(),
        review: vec![],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let inner = harness.workspace_root.join("inner");
    std::fs::create_dir(&inner).expect("inner dir");
    let outside = tempfile::tempdir().expect("outside");
    let response = submit(
        &harness,
        serde_json::json!({
            "command": "printf escaped > marker",
            "cwd": inner.to_string_lossy(),
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let command_id = body["data"]["id"].as_str().expect("command id").to_owned();

    std::fs::remove_dir(&inner).expect("remove inner");
    std::os::unix::fs::symlink(outside.path(), &inner).expect("replace with symlink");
    approve_pending_command(&harness, &command_id).await;

    let final_body = wait_for_terminal(&harness, &command_id).await;
    assert_eq!(final_body["data"]["status"], "failed");
    assert!(
        !outside.path().join("marker").exists(),
        "command must not run after cwd symlink replacement"
    );
}

#[tokio::test]
async fn cancel_transitions_running_command_to_cancelled() {
    let harness = Harness::spawn().await;
    let response = submit(&harness, serde_json::json!({"command": "sleep 30"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();

    // Give the supervisor a moment to mark the row running before we cancel.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let cancel =
        auth(session_client().post(format!("{}/v1/commands/{}/cancel", harness.base_url, id)))
            .send()
            .await
            .expect("send");
    assert_eq!(cancel.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "cancelled");

    let events = auth(session_client().get(format!(
        "{}/v1/logs/events?kind=command.cancelled&command_id={id}",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(events.status(), StatusCode::OK);
    let events_body: Value = events.json().await.expect("json");
    assert_eq!(events_body["data"]["events"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn timeout_marks_failed_status() {
    let commands = CommandsConfig {
        default_timeout: "300ms".to_owned(),
        cancel_grace: "200ms".to_owned(),
        env_allowlist: vec![],
        max_output_bytes: 1_048_576,
        ..CommandsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "sleep 30"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "failed");
}

#[tokio::test]
async fn output_truncation_marks_truncated_flag() {
    let commands = CommandsConfig {
        default_timeout: "10m".to_owned(),
        cancel_grace: "5s".to_owned(),
        env_allowlist: vec![],
        max_output_bytes: 16,
        ..CommandsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "sh -c 'printf ABCDEFGHIJ; printf KLMNOPQRSTUVWXYZ12345'"}),
    )
    .await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["truncated"], true);
}

#[tokio::test]
async fn command_output_endpoint_replays_chunks_with_cursor() {
    let harness = Harness::spawn().await;
    let response = submit(
        &harness,
        serde_json::json!({"command": "sh -c 'printf stdout-one; printf stderr-two >&2'"}),
    )
    .await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["last_output_seq"], 1);
    assert!(final_body["data"]["last_output_event_id"].is_string());
    assert!(final_body["data"]["last_output_at"].is_string());
    assert!(final_body["data"]["last_progress_at"].is_string());
    assert!(final_body["data"]["output_bytes"].as_i64().unwrap() >= 20);

    let first = auth(session_client().get(format!(
        "{}/v1/commands/{id}/output?order=asc&limit=1",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: Value = first.json().await.expect("json");
    let first_chunks = first_body["data"]["chunks"].as_array().unwrap();
    assert_eq!(first_chunks.len(), 1);
    let cursor = first_body["data"]["next_cursor"].as_str().unwrap();
    assert_eq!(first_chunks[0]["command_id"], id);
    assert!(first_chunks[0]["event_id"].is_string());
    assert!(first_chunks[0]["created_at"].is_string());
    assert!(first_chunks[0]["stream"] == "stdout" || first_chunks[0]["stream"] == "stderr");
    assert_eq!(first_chunks[0]["seq"], 0);

    let second = auth(session_client().get(format!(
        "{}/v1/commands/{id}/output?order=asc&after={cursor}",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(second.status(), StatusCode::OK);
    let second_body: Value = second.json().await.expect("json");
    let second_chunks = second_body["data"]["chunks"].as_array().unwrap();
    assert_eq!(second_chunks.len(), 1);
    assert_eq!(second_chunks[0]["command_id"], id);
    assert_eq!(second_chunks[0]["seq"], 1);
    assert_ne!(second_chunks[0]["event_id"], first_chunks[0]["event_id"]);
}

#[tokio::test]
async fn command_output_endpoint_isolates_unrelated_commands() {
    let harness = Harness::spawn().await;
    let first = submit(&harness, serde_json::json!({"command": "printf first"})).await;
    let first_body: Value = first.json().await.expect("json");
    let first_id = first_body["data"]["id"].as_str().unwrap().to_owned();
    let second = submit(&harness, serde_json::json!({"command": "printf second"})).await;
    let second_body: Value = second.json().await.expect("json");
    let second_id = second_body["data"]["id"].as_str().unwrap().to_owned();
    wait_for_terminal(&harness, &first_id).await;
    wait_for_terminal(&harness, &second_id).await;

    let output = auth(session_client().get(format!(
        "{}/v1/commands/{first_id}/output?order=asc",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(output.status(), StatusCode::OK);
    let body: Value = output.json().await.expect("json");
    let chunks = body["data"]["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["command_id"], first_id);
    assert_eq!(chunks[0]["data"], "first");
}

#[tokio::test]
async fn quiet_command_emits_progress_events() {
    let commands = CommandsConfig {
        progress_interval: "100ms".to_owned(),
        ..CommandsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "sleep 0.35"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert!(final_body["data"]["last_progress_at"].is_string());

    let events = auth(session_client().get(format!(
        "{}/v1/logs/events?kind=command.progress&command_id={id}",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(events.status(), StatusCode::OK);
    let events_body: Value = events.json().await.expect("json");
    assert!(
        !events_body["data"]["events"].as_array().unwrap().is_empty(),
        "expected at least one progress event"
    );
}

#[tokio::test]
async fn admin_key_rejected_on_session_route() {
    let harness = Harness::spawn().await;
    let response = admin_auth(session_client().post(format!("{}/v1/commands", harness.base_url)))
        .json(&serde_json::json!({"command": "echo"}))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn get_returns_not_found_for_unknown_id() {
    let harness = Harness::spawn().await;
    let response = auth(session_client().get(format!(
        "{}/v1/commands/cmd_does_not_exist",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.not_found");
}

#[tokio::test]
async fn list_returns_recent_commands() {
    let harness = Harness::spawn().await;
    for command in ["echo a", "echo b", "echo c"] {
        let response = submit(&harness, serde_json::json!({"command": command})).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    // Wait for all three to finish so list order is stable.
    for _ in 0..30 {
        let response = auth(session_client().get(format!("{}/v1/commands", harness.base_url)))
            .send()
            .await
            .expect("send");
        let body: Value = response.json().await.expect("json");
        let items = body["data"]["items"].as_array().expect("items");
        if items.iter().all(|item| {
            let status = item["status"].as_str().unwrap_or("");
            status != "pending" && status != "running"
        }) && items.len() == 3
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("commands did not finish in time");
}

#[tokio::test]
async fn truncated_noisy_command_still_emits_progress_events() {
    let commands = CommandsConfig {
        progress_interval: "50ms".to_owned(),
        max_output_bytes: 8,
        ..CommandsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: None,
        commands: Some(commands),
    })
    .await;
    let response = submit(
        &harness,
        serde_json::json!({
            "command": "sh -c 'i=0; while [ $i -lt 8 ]; do printf 1234567890; i=$((i+1)); sleep 0.03; done'"
        }),
    )
    .await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let final_body = wait_for_terminal(&harness, &id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["truncated"], true);

    let events = auth(session_client().get(format!(
        "{}/v1/logs/events?kind=command.progress&command_id={id}",
        harness.base_url
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(events.status(), StatusCode::OK);
    let events_body: Value = events.json().await.expect("json");
    assert!(
        !events_body["data"]["events"].as_array().unwrap().is_empty(),
        "expected progress events after output truncation"
    );
}

#[tokio::test]
async fn websocket_streams_command_stdout_and_exit() {
    let harness = Harness::spawn().await;
    // `commands.{id}` is per-row, so the id has to exist before subscribing.
    // We slow the command itself with `sleep 0.3` so the supervisor's events
    // (`command.started`, stdout chunk from `echo`, `command.exited`) fire
    // AFTER the WebSocket subscription is registered.
    let response = submit(
        &harness,
        serde_json::json!({"command": "sh -c 'sleep 0.3 && echo streamed'"}),
    )
    .await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let topic = format!("commands.{id}");
    let mut stream = open_ws(&harness.base_url, &[topic.as_str()]).await;
    // Subscribe is async; give the server a brief moment to register the
    // topic before the supervisor begins emitting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _final_body = wait_for_terminal(&harness, &id).await;
    let events = collect_until(&mut stream, |value| {
        value["payload"]["kind"]
            .as_str()
            .map(|kind| kind == "command.exited" || kind == "command.failed")
            .unwrap_or(false)
    })
    .await;
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|event| event["payload"]["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"command.exited") && kinds.contains(&"command.stdout"),
        "expected both command.stdout and command.exited, got: {kinds:?}"
    );
    let stdout = events
        .iter()
        .find(|event| event["payload"]["kind"] == "command.stdout")
        .expect("stdout event");
    let data = &stdout["payload"]["data"];
    assert!(data["event_id"].is_string());
    assert!(data["created_at"].is_string());
    assert_eq!(data["command_id"], id);
    assert_eq!(data["stream"], "stdout");
    assert!(data["seq"].is_number());
    assert!(data["data"].as_str().unwrap().contains("streamed"));
}

#[tokio::test]
async fn websocket_logs_topic_receives_every_event() {
    let harness = Harness::spawn().await;
    let mut stream = open_ws(&harness.base_url, &["logs"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let response = submit(&harness, serde_json::json!({"command": "echo log"})).await;
    let body: Value = response.json().await.expect("json");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    let _final_body = wait_for_terminal(&harness, &id).await;
    let events = collect_until(&mut stream, |value| {
        value["payload"]["kind"]
            .as_str()
            .map(|kind| kind == "command.exited")
            .unwrap_or(false)
    })
    .await;
    let topics: Vec<&str> = events
        .iter()
        .filter_map(|event| event["topic"].as_str())
        .collect();
    assert!(
        topics.contains(&"logs"),
        "expected at least one logs event, saw {topics:?}"
    );
}
