//! Sandbox workload identity error helpers (`sandbox.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        WorkloadUserUnresolved { .. } => "sandbox.workload_user_unresolved",
        WorkloadUserLookupFailed { .. } => "sandbox.workload_user_lookup_failed",
        WorkloadUserIsRuntime { .. } => "sandbox.workload_user_is_runtime",
        WorkloadUserIsRoot { .. } => "sandbox.workload_user_is_root",
        WorkloadUserSharesGroup { .. } => "sandbox.workload_user_shares_group",
        WorkloadUserModeUnsupported { .. } => "sandbox.workload_user_mode_unsupported",
        NetworkProviderRequired => "sandbox.network_provider_required",
        WorkloadUnreachable { .. } => "sandbox.workload_unreachable",
        WorkloadWritableExecutable { .. } => "sandbox.workload_writable_executable",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        WorkloadUserUnresolved { name } => {
            format!("sandbox workload user `{name}` does not resolve to a local user")
        }
        WorkloadUserLookupFailed { name, .. } => {
            format!("sandbox workload user `{name}` lookup failed")
        }
        WorkloadUserIsRuntime { name, .. } => {
            format!("sandbox workload user `{name}` is the runtime user")
        }
        WorkloadUserIsRoot { name } => format!("sandbox workload user `{name}` is root"),
        WorkloadUserSharesGroup { name, .. } => {
            format!(
                "sandbox workload user `{name}` shares its primary group with root or the runtime"
            )
        }
        WorkloadUserModeUnsupported { mode } => {
            format!("sandbox mode `{mode}` cannot run the workload as another user")
        }
        NetworkProviderRequired => {
            "sandbox requires a network-provider extension and none is declared".to_owned()
        }
        // `path` and `reason` carry local filesystem detail.
        WorkloadUnreachable { subject, .. } => {
            format!("{subject} is not reachable by the sandbox workload user")
        }
        WorkloadWritableExecutable { subject, .. } => {
            format!("{subject} is writable by the sandbox workload user")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        WorkloadUserModeUnsupported { .. } => StatusCode::BAD_REQUEST,
        WorkloadUserUnresolved { .. }
        | WorkloadUserLookupFailed { .. }
        | WorkloadUserIsRuntime { .. }
        | WorkloadUserIsRoot { .. }
        | WorkloadUserSharesGroup { .. }
        | NetworkProviderRequired
        | WorkloadUnreachable { .. }
        | WorkloadWritableExecutable { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
