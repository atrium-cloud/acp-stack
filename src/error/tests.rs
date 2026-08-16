use super::StackError;
use std::path::PathBuf;

#[test]
fn stack_update_binary_swap_reports_rollback_outcome() {
    let io = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    let clean = StackError::StackUpdateBinarySwap {
        path: PathBuf::from("/opt/acps/acps"),
        source: io(),
        rollback_errors: Vec::new(),
    };
    assert_eq!(clean.error_code(), "stack.update_binary_swap_failed");
    assert_eq!(
        clean.to_string(),
        "failed to replace /opt/acps/acps during stack update binary swap: denied"
    );

    let broken = StackError::StackUpdateBinarySwap {
        path: PathBuf::from("/opt/acps/acps"),
        source: io(),
        rollback_errors: vec!["failed to restore /opt/acps/acps: denied".to_owned()],
    };
    assert_eq!(broken.error_code(), "stack.update_binary_swap_failed");
    assert_eq!(
        broken.to_string(),
        "failed to replace /opt/acps/acps during stack update binary swap: denied; rollback errors: failed to restore /opt/acps/acps: denied"
    );
    assert!(!broken.public_message().contains("/opt"));
}

#[test]
fn workspace_command_failure_display_formats_exit_status_plainly() {
    let exited = StackError::WorkspaceCommandFailed {
        command: "git clone",
        exit: Some(128),
        stderr_tail: "repository not found".to_owned(),
    }
    .to_string();
    assert_eq!(
        exited,
        "`git clone` exited with status 128: repository not found"
    );
    assert!(
        !exited.contains("Some("),
        "exit status must not expose Option debug formatting: {exited}"
    );

    let signaled = StackError::WorkspaceCommandFailed {
        command: "git clone",
        exit: None,
        stderr_tail: "terminated by signal".to_owned(),
    }
    .to_string();
    assert_eq!(
        signaled,
        "`git clone` exited without a status: terminated by signal"
    );
}
