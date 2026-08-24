use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::common::HomeEnvGuard;
use crate::common::agent::{AgentHarness, admin_bearer, http, session_bearer};

// Between the 1 MiB per-file content cap and the ~6 MiB whole-request cap, so
// the request layer admits it and the handler rejects it.
const OVER_CONTENT_UNDER_REQUEST_BYTES: usize = 2 * 1_048_576;

/// The inspect route's `RequestBodyLimitLayer` is deliberately looser than the
/// content cap the handler enforces, so each size lands on its own rejection.
#[tokio::test]
async fn native_config_inspect_request_layer_defers_to_content_cap() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    let client = http().await;

    let over_content = "x".repeat(OVER_CONTENT_UNDER_REQUEST_BYTES);
    let response = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": over_content }))
        .send()
        .await
        .expect("send inspect");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = response.json().await.expect("inspect json");
    assert_eq!(body["error"]["code"], "agent.native_config_too_large");

    // The body-limit layer stops reading mid-send, so a reset/broken-pipe races
    // the 413; both count as the rejection observed from the socket.
    let over_request = "x".repeat(acp_stack::config::IMPORT_REQUEST_SIZE_LIMIT + 1_048_576);
    let result = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": over_request }))
        .send()
        .await;
    match result {
        Ok(response) => {
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let text = response.text().await.unwrap_or_default();
            if let Ok(body) = serde_json::from_str::<Value>(&text) {
                assert_ne!(
                    body["error"]["code"], "agent.native_config_too_large",
                    "oversize request should be rejected by the body-limit layer, not the content cap: {body}"
                );
            }
        }
        Err(error) => {
            // A crashed server would also land in this arm, so re-issue the
            // content-cap request to prove the server is alive and rejecting.
            let retry_content = "x".repeat(OVER_CONTENT_UNDER_REQUEST_BYTES);
            let response = client
                .post(format!(
                    "{}/v1/agent/config/native/inspect",
                    harness.base_url
                ))
                .header("Authorization", admin_bearer())
                .json(&json!({ "filename": "opencode.json", "content": retry_content }))
                .send()
                .await
                .unwrap_or_else(|retry_error| {
                    panic!("server unreachable after oversize send failed ({error}): {retry_error}")
                });
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let body: Value = response
                .json()
                .await
                .expect("inspect json after oversize send abort");
            assert_eq!(body["error"]["code"], "agent.native_config_too_large");
        }
    }
}

/// Drives the full inspect -> import -> cancel rollback loop over HTTP with a
/// model-free native config, so apply never launches an agent.
#[tokio::test]
async fn native_config_cancel_rolls_back_and_guards_digest() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    // The import prepare path opens the secret store read-only, so it must
    // exist under HOME even with no secret refs in play.
    acp_stack::secrets::SecretStore::open_or_create(&home).expect("secret store");
    let native_path = home.join(".config").join("opencode").join("opencode.json");
    let client = http().await;

    let rejected = client
        .post(format!(
            "{}/v1/agent/config/native/import/op_missing/cancel",
            harness.base_url
        ))
        .header("Authorization", session_bearer())
        .send()
        .await
        .expect("send session cancel");
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    let rejected_body: Value = rejected.json().await.expect("json");
    assert_eq!(rejected_body["error"]["code"], "auth.wrong_kind");

    let canonical_before =
        std::fs::read(&harness.config_path).expect("canonical config before import");

    let operation_id = apply_theme_import(&client, &harness.base_url).await;

    assert!(
        native_path.is_file(),
        "native file should exist after apply"
    );

    let cancel = client
        .post(format!(
            "{}/v1/agent/config/native/import/{operation_id}/cancel",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send cancel");
    let status = cancel.status();
    let body: Value = cancel.json().await.expect("cancel json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["status"], "cancelled");
    assert!(
        !native_path.exists(),
        "native file should be removed after cancel rollback"
    );
    let canonical_after =
        std::fs::read(&harness.config_path).expect("canonical config after cancel");
    assert_eq!(
        canonical_before, canonical_after,
        "canonical config bytes should be restored by rollback"
    );

    // Digest guard: cancel must refuse rather than roll back over a native
    // file that was tampered with after apply.
    let guarded_operation_id = apply_theme_import(&client, &harness.base_url).await;
    assert!(
        native_path.is_file(),
        "native file should exist after apply"
    );
    let mut mutated = std::fs::read(&native_path).expect("read applied native file");
    mutated.extend_from_slice(b"\n// tampered\n");
    std::fs::write(&native_path, &mutated).expect("mutate applied native file");

    let guarded_cancel = client
        .post(format!(
            "{}/v1/agent/config/native/import/{guarded_operation_id}/cancel",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .send()
        .await
        .expect("send guarded cancel");
    let guarded_status = guarded_cancel.status();
    let guarded_body: Value = guarded_cancel.json().await.expect("guarded cancel json");
    assert_eq!(guarded_status, StatusCode::CONFLICT, "body: {guarded_body}");
    assert_eq!(
        guarded_body["error"]["code"],
        "agent.native_config_rollback_conflict"
    );
}

#[tokio::test]
async fn native_config_import_serializes_with_agent_config_mutation_lock() {
    let harness = AgentHarness::spawn().await;
    let home = harness
        .config_path
        .parent()
        .expect("config path has parent")
        .to_path_buf();
    let _home = HomeEnvGuard::set(&home);
    acp_stack::secrets::SecretStore::open_or_create(&home).expect("secret store");
    let client = http().await;

    let inspect = client
        .post(format!(
            "{}/v1/agent/config/native/inspect",
            harness.base_url
        ))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": r#"{"theme":"dark"}"# }))
        .send()
        .await
        .expect("send inspect");
    let inspect_body: Value = inspect.json().await.expect("inspect json");
    let revision = inspect_body["data"]["revision"]
        .as_str()
        .expect("inspect revision")
        .to_owned();

    // Hold the cross-process mutation lock the serialized writers share; the
    // import must block on it and only complete after release.
    let lock = acp_stack::fs_util::acquire_agent_config_mutation_file_lock(&harness.config_path)
        .expect("acquire mutation lock");
    let import_client = client.clone();
    let base_url = harness.base_url.clone();
    let import_task = tokio::spawn(async move {
        import_client
            .post(format!("{base_url}/v1/agent/config/native/import"))
            .header("Authorization", admin_bearer())
            .json(&json!({
                "revision": revision,
                "selected_managed_field_ids": [],
                "executable_settings_acknowledged": false
            }))
            .send()
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(
        !import_task.is_finished(),
        "import must wait while the mutation lock is held"
    );
    drop(lock);
    let import = import_task
        .await
        .expect("join import task")
        .expect("send import");
    let status = import.status();
    let body: Value = import.json().await.expect("import json");
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["data"]["status"], "applied", "body: {body}");
}

/// Inspect `{"theme":"dark"}` then import the empty selection, returning the
/// applied operation id.
async fn apply_theme_import(client: &reqwest::Client, base_url: &str) -> String {
    let inspect = client
        .post(format!("{base_url}/v1/agent/config/native/inspect"))
        .header("Authorization", admin_bearer())
        .json(&json!({ "filename": "opencode.json", "content": r#"{"theme":"dark"}"# }))
        .send()
        .await
        .expect("send inspect");
    let inspect_status = inspect.status();
    let inspect_body: Value = inspect.json().await.expect("inspect json");
    assert_eq!(inspect_status, StatusCode::OK, "body: {inspect_body}");
    let revision = inspect_body["data"]["revision"]
        .as_str()
        .expect("inspect revision")
        .to_owned();

    let import = client
        .post(format!("{base_url}/v1/agent/config/native/import"))
        .header("Authorization", admin_bearer())
        .json(&json!({
            "revision": revision,
            "selected_managed_field_ids": [],
            "executable_settings_acknowledged": false
        }))
        .send()
        .await
        .expect("send import");
    let import_status = import.status();
    let import_body: Value = import.json().await.expect("import json");
    assert_eq!(import_status, StatusCode::OK, "body: {import_body}");
    assert_eq!(
        import_body["data"]["status"], "applied",
        "body: {import_body}"
    );
    assert_eq!(
        import_body["data"]["restart"]["required"], false,
        "no running agent, so no restart required: {import_body}"
    );
    import_body["data"]["operation_id"]
        .as_str()
        .expect("operation id")
        .to_owned()
}
