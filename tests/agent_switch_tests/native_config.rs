use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::common::HomeEnvGuard;
use crate::common::agent::{AgentHarness, admin_bearer, http, session_bearer};

// Content string that sits between the 1 MiB per-file content cap and the
// ~6 MiB whole-request cap, so the request layer admits it and the handler
// rejects it on the content cap.
const OVER_CONTENT_UNDER_REQUEST_BYTES: usize = 2 * 1_048_576;

/// The inspect route is layered with `RequestBodyLimitLayer(IMPORT_REQUEST_SIZE_LIMIT)`,
/// which is deliberately looser than the 1 MiB content cap the handler enforces.
/// A ~2 MiB content string must reach the handler and fail on the content cap;
/// a content string large enough to blow past the ~6 MiB request cap must be
/// rejected at the body-limit layer before the handler runs.
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

    // Between the content cap and the request cap: reaches the handler, fails
    // on the content cap with `native_config_too_large` (HTTP 413).
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

    // Past the whole-request cap: rejected at the body-limit layer. The
    // middleware response may not be JSON, so assert on status and only on
    // the envelope if the body parses as JSON. The layer also stops reading
    // and may abort the connection before the client finishes writing the
    // oversize body, so a reset/broken-pipe mid-send races the 413 response;
    // both are the rejection observed from the socket.
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
            // The abort can surface as a reset, broken pipe, or hyper's
            // sourceless "incomplete message", so no error shape is asserted
            // here. A crashed server would also land in this arm, so prove
            // the server is still alive and rejecting by re-issuing the
            // content-cap request and re-asserting its typed 413.
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

/// Drives the full inspect -> import -> cancel rollback loop through the HTTP
/// layer with a model-free native config, so apply never triggers model
/// discovery or an agent launch. Covers the happy-path rollback and the
/// digest-guard rejection when the applied native file is mutated on disk,
/// plus admin-tier enforcement on the cancel route.
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
    // exist under HOME even though `{"theme":"dark"}` carries no secret refs.
    acp_stack::secrets::SecretStore::open_or_create(&home).expect("secret store");
    let native_path = home.join(".config").join("opencode").join("opencode.json");
    let client = http().await;

    // Admin-tier enforcement: the cancel route rejects a session key with
    // `auth.wrong_kind` (401), matching the other admin-route tests here.
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

    // Applied without a running agent: no restart required, native file on disk.
    assert!(
        native_path.is_file(),
        "native file should exist after apply"
    );

    // Happy path: cancel rolls back, dropping the native file and restoring
    // the canonical config bytes verbatim.
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

    // Digest guard: a fresh apply, then mutate the applied native file on
    // disk. Cancel must refuse with `native_config_rollback_conflict` (409)
    // rather than roll back over the tampered file.
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

    // Hold the cross-process mutation lock; the import must block on it and
    // only complete after release. `acps agent set` and the other serialized
    // writers take the same lock, so this pins the import side of the pairing.
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
/// applied operation id. The theme-only config carries no model key, so apply
/// never triggers model discovery or an agent launch.
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
