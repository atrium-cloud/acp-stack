//! Command policy coverage: deny/review pattern matching against shell-obfuscated
//! command words and composed segments, plus the supervised/locked
//! approve/deny quadrants and the permission events they emit.

mod common;

use acp_stack::config::PermissionsConfig;
use common::commands::{
    Harness, HarnessOverrides, auth, pending_permission_for_command, session_client, submit,
    wait_for_terminal,
};
use reqwest::StatusCode;
use serde_json::Value;

#[tokio::test]
async fn deny_pattern_rejects_submission() {
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
    let response = submit(&harness, serde_json::json!({"command": "rm -rf /"})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_shell_constructed_command_word() {
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
    let response = submit(&harness, serde_json::json!({"command": "r''m -rf target"})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_escaped_newline_command_word() {
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
        serde_json::json!({"command": "r\\\nm -rf target"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_ansi_c_quoted_command_word() {
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
    let response = submit(&harness, serde_json::json!({"command": "$'r'm -rf target"})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_ansi_c_octal_quoted_command_word() {
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
        serde_json::json!({"command": r"$'\162'm -rf target"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_ansi_c_nul_quoted_command_word() {
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
        serde_json::json!({"command": r"$'rm\0' -rf target"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_ansi_c_nul_suffix_quoted_command_word() {
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
        serde_json::json!({"command": r"$'rm\0suffix' -rf target"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_shell_constructed_command_after_assignment_prefix() {
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
        serde_json::json!({"command": "X=1 r''m -rf target"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_shell_constructed_command_after_time_prefix() {
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
        serde_json::json!({"command": "time r''m -rf target"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_shell_constructed_command_after_redirection_prefix() {
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
    let redirect_path = harness.workspace_root.join("policy.log");
    let response = submit(
        &harness,
        serde_json::json!({
            "command": format!(">{} r''m -rf target", redirect_path.to_string_lossy()),
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_composed_segment() {
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
        serde_json::json!({"command": "true && rm -rf target"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_command_substitution_segment() {
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
        serde_json::json!({"command": r#"echo "$(rm -rf target)""#}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn deny_pattern_rejects_process_substitution_segment() {
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
        serde_json::json!({"command": "cat <(rm -rf target)"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], "command.denied");
}

#[tokio::test]
async fn review_pattern_enqueues_permission_in_supervised_mode() {
    let permissions = PermissionsConfig {
        mode: "supervised".to_owned(),
        review: vec!["sudo *".to_owned()],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "sudo apt update"})).await;
    // Row is created in pending state; permission decision lands out-of-band.
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
        .find(|p| p["subject_id"].as_str() == Some(&cmd_id))
        .expect("pending permission row for command");
    let perm_id = entry["id"].as_str().unwrap().to_owned();

    let deny_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/deny",
        harness.base_url, perm_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(deny_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &cmd_id).await;
    assert_eq!(final_body["data"]["status"], "failed");
    assert_eq!(final_body["data"]["exit_status"], Value::Null);
}

#[tokio::test]
async fn canceling_command_awaiting_permission_settles_both_rows() {
    let permissions = PermissionsConfig {
        mode: "supervised".to_owned(),
        review: vec!["sudo *".to_owned()],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "sudo apt update"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let command_id = body["data"]["id"].as_str().expect("command id").to_owned();
    let permission = pending_permission_for_command(&harness, &command_id).await;
    let permission_id = permission["id"].as_str().expect("permission id").to_owned();

    let cancel_response = auth(session_client().post(format!(
        "{}/v1/commands/{}/cancel",
        harness.base_url, command_id
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(cancel_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &command_id).await;
    assert_eq!(final_body["data"]["status"], "cancelled");

    // No orphan: the permission row settles with the command...
    let permission_response = auth(session_client().get(format!(
        "{}/v1/permissions/{}",
        harness.base_url, permission_id
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(permission_response.status(), StatusCode::OK);
    let permission_body: Value = permission_response.json().await.expect("json");
    assert_eq!(permission_body["data"]["status"], "cancelled");

    // ...and approving it afterwards is a state conflict, not a bad request.
    let approve_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/approve",
        harness.base_url, permission_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(approve_response.status(), StatusCode::CONFLICT);
    let approve_body: Value = approve_response.json().await.expect("json");
    assert_eq!(
        approve_body["error"]["code"],
        "permission.invalid_transition"
    );

    // The cancellation is visible in the command's own event stream, with a
    // reason naming the cause.
    let events_response = auth(session_client().get(format!(
        "{}/v1/logs/events?command_id={}&limit=50",
        harness.base_url, command_id
    )))
    .send()
    .await
    .expect("send");
    assert_eq!(events_response.status(), StatusCode::OK);
    let events_body: Value = events_response.json().await.expect("json");
    let events = events_body["data"]["events"]
        .as_array()
        .expect("events array");
    let canceled_event = events
        .iter()
        .find(|event| event["kind"] == "permission.cancelled")
        .expect("permission.cancelled event in command stream");
    let payload: Value = serde_json::from_str(
        canceled_event["payload_json"]
            .as_str()
            .expect("payload_json"),
    )
    .expect("payload json");
    assert_eq!(payload["reason"], "command-cancelled");
    assert_eq!(payload["command_id"], command_id);
}

#[tokio::test]
async fn review_pattern_matches_shell_constructed_command_word() {
    let permissions = PermissionsConfig {
        mode: "supervised".to_owned(),
        review: vec!["sudo *".to_owned()],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "s''udo true"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let command_id = body["data"]["id"].as_str().expect("command id");
    let permission = pending_permission_for_command(&harness, command_id).await;
    assert_eq!(permission["detail"]["policy_decision"], "review");
}

#[tokio::test]
async fn review_pattern_in_process_substitution_enqueues_permission() {
    let permissions = PermissionsConfig {
        mode: "supervised".to_owned(),
        review: vec!["sudo *".to_owned()],
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
        serde_json::json!({"command": "diff <(sudo cat /etc/shadow) /dev/null"}),
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
        .expect("pending permission row for process substitution");
    assert_eq!(entry["detail"]["policy_decision"], "review");
}

#[tokio::test]
async fn locked_mode_enqueues_permission_and_approval_runs() {
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
    let response = submit(&harness, serde_json::json!({"command": "echo hi"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    let pending_body: Value = pending.json().await.expect("json");
    let permissions_list = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array");
    let perm_id = permissions_list
        .iter()
        .find(|p| p["subject_id"].as_str() == Some(&cmd_id))
        .expect("permission row")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let approve_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/approve",
        harness.base_url, perm_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(approve_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &cmd_id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["exit_status"], 0);

    // GET /v1/logs/permissions must surface the durable permission.* events
    // generated for this command's lifecycle (created + approved). Without
    // this assertion, a regression in PermissionService event persistence
    // would silently leave the log route returning an empty array.
    let logs = auth(session_client().get(format!("{}/v1/logs/permissions", harness.base_url)))
        .send()
        .await
        .expect("send");
    assert_eq!(logs.status(), StatusCode::OK);
    let logs_body: Value = logs.json().await.expect("json");
    let kinds: Vec<&str> = logs_body["data"]["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"permission.created"),
        "expected permission.created event, saw: {kinds:?}",
    );
    assert!(
        kinds.contains(&"permission.approved"),
        "expected permission.approved event, saw: {kinds:?}",
    );
}

#[tokio::test]
async fn review_supervised_mode_approve_runs_command() {
    // Quadrant: supervised + review-match + APPROVE → command transitions
    // to running and exits cleanly. Complements the existing supervised-deny
    // and locked-approve tests so all four review/locked outcomes are
    // covered end-to-end.
    let permissions = PermissionsConfig {
        mode: "supervised".to_owned(),
        review: vec!["echo *".to_owned()],
        deny: vec![],
        ..PermissionsConfig::default()
    };
    let harness = Harness::spawn_with(HarnessOverrides {
        permissions: Some(permissions),
        commands: None,
    })
    .await;
    let response = submit(&harness, serde_json::json!({"command": "echo allowed"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["status"], "pending");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    let pending_body: Value = pending.json().await.expect("json");
    let perm_id = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array")
        .iter()
        .find(|p| p["subject_id"].as_str() == Some(&cmd_id))
        .expect("permission row")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let approve_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/approve",
        harness.base_url, perm_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(approve_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &cmd_id).await;
    assert_eq!(final_body["data"]["status"], "exited");
    assert_eq!(final_body["data"]["exit_status"], 0);
}

#[tokio::test]
async fn locked_mode_deny_marks_command_failed() {
    // Quadrant: locked + DENY → command transitions to failed without ever
    // spawning a child. Completes the four-quadrant policy matrix.
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
    let response = submit(&harness, serde_json::json!({"command": "echo blocked"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    let cmd_id = body["data"]["id"].as_str().unwrap().to_owned();

    let pending =
        auth(session_client().get(format!("{}/v1/permissions/pending", harness.base_url)))
            .send()
            .await
            .expect("send");
    let pending_body: Value = pending.json().await.expect("json");
    let perm_id = pending_body["data"]["permissions"]
        .as_array()
        .expect("permissions array")
        .iter()
        .find(|p| p["subject_id"].as_str() == Some(&cmd_id))
        .expect("permission row")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let deny_response = auth(session_client().post(format!(
        "{}/v1/permissions/{}/deny",
        harness.base_url, perm_id
    )))
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send");
    assert_eq!(deny_response.status(), StatusCode::OK);

    let final_body = wait_for_terminal(&harness, &cmd_id).await;
    assert_eq!(final_body["data"]["status"], "failed");
    // exit_status is null because the child never ran.
    assert_eq!(final_body["data"]["exit_status"], Value::Null);

    // GET /v1/logs/permissions must surface permission.denied for this row.
    let logs = auth(session_client().get(format!("{}/v1/logs/permissions", harness.base_url)))
        .send()
        .await
        .expect("send");
    let logs_body: Value = logs.json().await.expect("json");
    let kinds: Vec<&str> = logs_body["data"]["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"permission.denied"),
        "expected permission.denied event, saw: {kinds:?}",
    );
}
