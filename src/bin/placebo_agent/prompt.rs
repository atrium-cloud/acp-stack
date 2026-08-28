use super::*;

pub(crate) async fn handle_prompt(
    state: SharedState,
    request: PromptRequest,
    responder: Responder<PromptResponse>,
    connection: ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    // Captured before any await that could observe a cancel: a `session/cancel` counts
    // against this turn only if it arrives after this point, which is what lets a later
    // turn on the same session shrug off a cancel aimed at an earlier one.
    let (args, start_cancels) = {
        let state = state.lock().await;
        (
            state.args.clone(),
            state.cancel_count(request.session_id.0.as_ref()),
        )
    };
    if args.prompt_error {
        return responder.respond_with_error(Error::new(-32000, "fake prompt failure"));
    }
    if let Some(message) = args.prompt_inference_error {
        return responder.respond_with_error(Error::new(-32000, message));
    }
    if args.request_permission_then_cancel {
        let permission_request = RequestPermissionRequest::new(
            request.session_id.clone(),
            ToolCallUpdate::new("tool_permission_cancel", ToolCallUpdateFields::new()),
            vec![PermissionOption::new(
                "allow",
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
        );
        let permission = connection.send_request(permission_request);
        permission.cancel()?;
        let state_for_task = Arc::clone(&state);
        return connection.spawn(async move {
            let Some(error) = permission.block_task().await.err() else {
                return responder.respond_with_error(Error::new(
                    -32000,
                    "cancelled permission returned a successful response",
                ));
            };
            if error.code != agent_client_protocol::ErrorCode::RequestCancelled {
                return responder.respond_with_error(Error::new(
                    -32000,
                    format!("cancelled permission returned error {}", error.code),
                ));
            }
            finish_prompt(state_for_task, request, responder).await
        });
    }
    {
        let state = state.lock().await;
        if state.args.expect_model_config.is_some() && !state.model_configured {
            return responder
                .respond_with_error(Error::new(-32000, "expected model config before prompt"));
        }
        if state.args.expect_mode.is_some() && !state.mode_configured {
            return responder
                .respond_with_error(Error::new(-32000, "expected session mode before prompt"));
        }
    }
    if prompt_contains_testflight_marker(&request) {
        tokio::fs::write(TESTFLIGHT_MARKER, TESTFLIGHT_CONTENT)
            .await
            .map_err(Error::into_internal_error)?;
    }
    if !args.prompt_silent {
        let chunks: &[&str] = if args.prompt_stall_after_update {
            &[FIRST_CHUNK]
        } else {
            &[FIRST_CHUNK, SECOND_CHUNK]
        };
        for text in chunks {
            connection.send_notification(SessionNotification::new(
                request.session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(*text),
                ))),
            ))?;
        }
    }
    // The two off-loop branches below keep the dispatch loop free, so the
    // client's `session/cancel` is processed while the turn is still open.
    // An inline await (as `--prompt-stall-after-update` does) would park the
    // loop instead and the notification would never be read.
    if args.prompt_never_settle {
        return connection.spawn(async move {
            let _responder_held_open = responder;
            loop {
                tokio::time::sleep(STALL_SLEEP).await;
            }
        });
    }
    if let Some(delay_ms) = args.prompt_settle_cancel_after_ms {
        let state_for_task = Arc::clone(&state);
        return connection.spawn(async move {
            // Wait for a cancel aimed at this turn: one that arrives after it started,
            // lifting the session's count past the value captured at start. The check
            // is non-consuming and epoch-gated, so every turn parked at cancel time
            // settles while a turn that starts later does not inherit that cancel.
            loop {
                let cancelled = {
                    let state = state_for_task.lock().await;
                    state.cancelled_since(request.session_id.0.as_ref(), start_cancels)
                };
                if cancelled {
                    break;
                }
                tokio::time::sleep(CANCEL_WAIT_POLL_INTERVAL).await;
            }
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            respond_to_prompt(request, responder, StopReason::Cancelled)
        });
    }
    if args.prompt_stall_after_update {
        loop {
            tokio::time::sleep(STALL_SLEEP).await;
        }
    }
    if let Some(delay_ms) = args.prompt_response_delay_ms {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    if let Some(message) = args.prompt_inference_error_after_update {
        return responder.respond_with_error(Error::new(-32000, message));
    }
    // The client probes send requests back to the client, so they must run off the event loop:
    // block_task inside a handler deadlocks.
    let probe_requested = args.terminal_command.is_some()
        || args.terminal_release_unknown
        || args.fs_write_path.is_some()
        || args.fs_read_path.is_some();
    if probe_requested {
        let (terminal_advertised, fs_advertised) = {
            let state = state.lock().await;
            let caps = state.client_capabilities.as_ref();
            (
                caps.is_some_and(|caps| caps.terminal),
                caps.is_some_and(|caps| caps.fs.read_text_file && caps.fs.write_text_file),
            )
        };
        let state_for_task = Arc::clone(&state);
        let probe_connection = connection.clone();
        return connection.spawn(async move {
            let terminal_report = run_terminal_probe(
                &args,
                &request.session_id,
                &probe_connection,
                terminal_advertised,
            )
            .await;
            let report = match terminal_report {
                Ok(mut report) => {
                    match run_fs_probe(&args, &request.session_id, &probe_connection, fs_advertised)
                        .await
                    {
                        Ok(fs_report) => {
                            report.extend(fs_report);
                            report
                        }
                        Err(error) => return responder.respond_with_error(error),
                    }
                }
                Err(error) => return responder.respond_with_error(error),
            };
            let report = serde_json::Value::Object(report);
            probe_connection.send_notification(SessionNotification::new(
                request.session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(format!("terminal-report:{report}")),
                ))),
            ))?;
            finish_prompt(state_for_task, request, responder).await
        });
    }
    finish_prompt(state, request, responder).await
}

async fn finish_prompt(
    state: SharedState,
    request: PromptRequest,
    responder: Responder<PromptResponse>,
) -> agent_client_protocol::Result<()> {
    // The inline path consumes a pending cancel: a cancel with no live turn is claimed
    // by the next turn to complete and removed, so it settles this turn as cancelled
    // without leaking onto the one after. The off-loop settle fixture uses the epoch
    // count instead; the two modes never run in the same process.
    let stop_reason = {
        let mut state = state.lock().await;
        if state
            .cancelled_sessions
            .remove(request.session_id.0.as_ref())
        {
            StopReason::Cancelled
        } else {
            StopReason::EndTurn
        }
    };
    respond_to_prompt(request, responder, stop_reason)
}

fn respond_to_prompt(
    request: PromptRequest,
    responder: Responder<PromptResponse>,
    stop_reason: StopReason,
) -> agent_client_protocol::Result<()> {
    // Echo the local message-id extension: acp-stack treats `_meta.acpStack.messageId` on the
    // response as the acknowledgment of the one it stamped on `session/prompt`.
    let mut response = PromptResponse::new(stop_reason);
    if let Some(message_id) = request
        .meta
        .as_ref()
        .and_then(|meta| meta.get("acpStack"))
        .and_then(|stack| stack.get("messageId"))
        .and_then(serde_json::Value::as_str)
    {
        let mut stack = serde_json::Map::new();
        stack.insert(
            "messageId".to_owned(),
            serde_json::Value::String(message_id.to_owned()),
        );
        let mut meta = serde_json::Map::new();
        meta.insert("acpStack".to_owned(), serde_json::Value::Object(stack));
        response = response.meta(meta);
    }
    responder.respond(response)
}

fn prompt_contains_testflight_marker(request: &PromptRequest) -> bool {
    request
        .prompt
        .iter()
        .any(content_contains_testflight_marker)
}

fn content_contains_testflight_marker(content: &ContentBlock) -> bool {
    match content {
        ContentBlock::Text(text) => text.text.contains(TESTFLIGHT_MARKER),
        ContentBlock::ResourceLink(link) => {
            link.uri.contains(TESTFLIGHT_MARKER) || link.name.contains(TESTFLIGHT_MARKER)
        }
        ContentBlock::Resource(_) => false,
        ContentBlock::Image(_) | ContentBlock::Audio(_) => false,
        _ => false,
    }
}
