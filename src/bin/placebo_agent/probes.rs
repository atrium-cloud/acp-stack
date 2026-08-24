use super::*;

/// `fs/*` round-trip driven against the client. Runs in a spawned task.
pub(crate) async fn run_fs_probe(
    args: &AcpArgs,
    session_id: &SessionId,
    connection: &ConnectionTo<Client>,
    fs_advertised: bool,
) -> agent_client_protocol::Result<serde_json::Map<String, serde_json::Value>> {
    let mut report = serde_json::Map::new();
    if args.fs_write_path.is_none() && args.fs_read_path.is_none() {
        return Ok(report);
    }
    if args.require_fs && !fs_advertised {
        report.insert(
            "fs_skipped".to_owned(),
            serde_json::json!("fs-not-advertised"),
        );
        return Ok(report);
    }
    if let Some(path) = &args.fs_write_path {
        let write = WriteTextFileRequest::new(
            session_id.clone(),
            path.clone(),
            args.fs_write_content.clone(),
        );
        match connection.send_request(write).block_task().await {
            Ok(_) => {
                report.insert("fs_write_ok".to_owned(), serde_json::json!(true));
            }
            Err(error) => {
                report.insert(
                    "fs_write_error_code".to_owned(),
                    serde_json::json!(error.code),
                );
            }
        }
    }
    if let Some(path) = &args.fs_read_path {
        let mut read = ReadTextFileRequest::new(session_id.clone(), path.clone());
        if let Some(line) = args.fs_read_line {
            read = read.line(line);
        }
        if let Some(limit) = args.fs_read_limit {
            read = read.limit(limit);
        }
        match connection.send_request(read).block_task().await {
            Ok(response) => {
                report.insert(
                    "fs_read_content".to_owned(),
                    serde_json::json!(response.content),
                );
            }
            Err(error) => {
                report.insert(
                    "fs_read_error_code".to_owned(),
                    serde_json::json!(error.code),
                );
            }
        }
    }
    Ok(report)
}

/// Terminal round-trip driven against the client. Runs in a spawned task, so `block_task` is safe.
pub(crate) async fn run_terminal_probe(
    args: &AcpArgs,
    session_id: &SessionId,
    connection: &ConnectionTo<Client>,
    terminal_advertised: bool,
) -> agent_client_protocol::Result<serde_json::Map<String, serde_json::Value>> {
    let mut report = serde_json::Map::new();
    if args.terminal_release_unknown {
        let error = connection
            .send_request(ReleaseTerminalRequest::new(
                session_id.clone(),
                TerminalId::new("term_unknown"),
            ))
            .block_task()
            .await
            .err();
        report.insert(
            "release_unknown_error_code".to_owned(),
            serde_json::json!(error.map(|error| error.code)),
        );
    }
    let Some(command) = &args.terminal_command else {
        return Ok(report);
    };
    if args.require_terminal && !terminal_advertised {
        report.insert(
            "skipped".to_owned(),
            serde_json::json!("terminal-not-advertised"),
        );
        return Ok(report);
    }
    let mut create = CreateTerminalRequest::new(session_id.clone(), command.clone())
        .args(args.terminal_arg.clone());
    if let Some(limit) = args.terminal_byte_limit {
        create = create.output_byte_limit(limit);
    }
    if let Some(cwd) = &args.terminal_cwd {
        create = create.cwd(cwd.clone());
    }
    let created = match connection.send_request(create).block_task().await {
        Ok(created) => created,
        Err(error) => {
            report.insert(
                "create_error_code".to_owned(),
                serde_json::json!(error.code),
            );
            return Ok(report);
        }
    };
    let terminal_id = created.terminal_id;
    if args.terminal_orphan {
        report.insert("orphaned".to_owned(), serde_json::json!(true));
        return Ok(report);
    }
    if args.terminal_cancel_wait {
        let wait = connection.send_request(WaitForTerminalExitRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ));
        wait.cancel()?;
        let error = wait.block_task().await.err();
        report.insert(
            "cancelled_wait_error_code".to_owned(),
            serde_json::json!(error.map(|error| error.code)),
        );
        let output = connection
            .send_request(TerminalOutputRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .block_task()
            .await?;
        report.insert(
            "output_after_cancel_ok".to_owned(),
            serde_json::json!(!output.truncated),
        );
    }
    if args.terminal_kill || args.terminal_cancel_wait {
        // Wait for child output before killing, so the read-after-kill assertion is deterministic.
        for _ in 0..100 {
            let output = connection
                .send_request(TerminalOutputRequest::new(
                    session_id.clone(),
                    terminal_id.clone(),
                ))
                .block_task()
                .await?;
            if !output.output.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        connection
            .send_request(KillTerminalRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .block_task()
            .await?;
    }
    let exit = connection
        .send_request(WaitForTerminalExitRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .block_task()
        .await?;
    report.insert(
        "exit_code".to_owned(),
        serde_json::json!(exit.exit_status.exit_code),
    );
    report.insert(
        "signal".to_owned(),
        serde_json::json!(exit.exit_status.signal),
    );
    let output = connection
        .send_request(TerminalOutputRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .block_task()
        .await?;
    report.insert("output".to_owned(), serde_json::json!(output.output));
    report.insert("truncated".to_owned(), serde_json::json!(output.truncated));
    connection
        .send_request(ReleaseTerminalRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .block_task()
        .await?;
    let post_release_error = connection
        .send_request(TerminalOutputRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .block_task()
        .await
        .err();
    report.insert(
        "post_release_error_code".to_owned(),
        serde_json::json!(post_release_error.map(|error| error.code)),
    );
    Ok(report)
}
