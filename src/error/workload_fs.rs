//! Workload-identity filesystem error helpers (`workload_fs.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        WorkloadFsInvalidPath { .. } => "workload_fs.invalid_path",
        WorkloadFsSymlinkRefused { .. } => "workload_fs.symlink_refused",
        WorkloadFsHardLinkRefused { .. } => "workload_fs.hard_link_refused",
        WorkloadFsNotRegular { .. } => "workload_fs.not_regular",
        WorkloadFsNotFound { .. } => "workload_fs.not_found",
        WorkloadFsAlreadyExists { .. } => "workload_fs.already_exists",
        WorkloadFsDestinationNotEmpty { .. } => "workload_fs.destination_not_empty",
        WorkloadFsOwnerMismatch { .. } => "workload_fs.owner_mismatch",
        WorkloadFsSymlinkLoop { .. } => "workload_fs.symlink_loop",
        WorkloadFsWorkloadWritable { .. } => "workload_fs.workload_writable",
        WorkloadFsWorkloadUnreadable { .. } => "workload_fs.workload_unreadable",
        WorkloadFsCredentialsFailed { .. } => "workload_fs.credentials_failed",
        WorkloadFsExecutorFailed { .. } => "workload_fs.executor_failed",
        WorkloadFsTimeout { .. } => "workload_fs.timeout",
        WorkloadFsIo { .. } => "workload_fs.io_failed",
        _ => return None,
    })
}

/// Every variant carries a host path or OS error text in `Display`; only the
/// static `reason` of `WorkloadFsInvalidPath` crosses the API boundary.
pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        WorkloadFsInvalidPath { reason, .. } => format!("workload path is invalid: {reason}"),
        WorkloadFsSymlinkRefused { .. } => "workload path contains a symlink".to_owned(),
        WorkloadFsHardLinkRefused { .. } => "workload file has more than one hard link".to_owned(),
        WorkloadFsNotRegular { .. } => "workload path is not a regular file".to_owned(),
        WorkloadFsNotFound { .. } => "workload path was not found".to_owned(),
        WorkloadFsAlreadyExists { .. } => "workload path already exists".to_owned(),
        WorkloadFsDestinationNotEmpty { .. } => {
            "workload destination exists and is not an empty directory".to_owned()
        }
        WorkloadFsOwnerMismatch { .. } => "workload file is owned by an unexpected user".to_owned(),
        WorkloadFsSymlinkLoop { .. } => "symlink chain loops or is too long".to_owned(),
        WorkloadFsWorkloadWritable { .. } => {
            "a trusted path is writable by the workload identity".to_owned()
        }
        WorkloadFsWorkloadUnreadable { .. } => {
            "a trusted path is not readable and executable by the workload identity".to_owned()
        }
        WorkloadFsCredentialsFailed { .. } => {
            "failed to enter workload filesystem credentials".to_owned()
        }
        WorkloadFsExecutorFailed { .. } => "workload filesystem job failed".to_owned(),
        WorkloadFsTimeout { .. } => "workload filesystem job timed out".to_owned(),
        WorkloadFsIo { .. } => "workload filesystem I/O failed".to_owned(),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        WorkloadFsInvalidPath { .. }
        | WorkloadFsSymlinkRefused { .. }
        | WorkloadFsHardLinkRefused { .. }
        | WorkloadFsNotRegular { .. }
        | WorkloadFsOwnerMismatch { .. } => StatusCode::BAD_REQUEST,
        WorkloadFsNotFound { .. } => StatusCode::NOT_FOUND,
        WorkloadFsAlreadyExists { .. } | WorkloadFsDestinationNotEmpty { .. } => {
            StatusCode::CONFLICT
        }
        WorkloadFsSymlinkLoop { .. }
        | WorkloadFsWorkloadWritable { .. }
        | WorkloadFsWorkloadUnreadable { .. }
        | WorkloadFsCredentialsFailed { .. }
        | WorkloadFsExecutorFailed { .. }
        | WorkloadFsTimeout { .. }
        | WorkloadFsIo { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
