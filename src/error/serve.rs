//! HTTP serve / process-startup error helpers (`serve.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        ServeBind { .. } => "serve.bind_failed",
        ServeIo { .. } => "serve.io_error",
        ServeRefusedAsRoot => "serve.refused_as_root",
        ServeRootRequiresAdminKey => "serve.root_requires_admin_key",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        ServeBind { .. } => "failed to bind HTTP listener".to_owned(),
        ServeIo { .. } => "HTTP server error".to_owned(),
        ServeRefusedAsRoot => "refusing to run as root without explicit opt-in".to_owned(),
        ServeRootRequiresAdminKey => {
            "running as root requires a non-empty admin API key".to_owned()
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        ServeBind { .. } | ServeIo { .. } | ServeRefusedAsRoot | ServeRootRequiresAdminKey => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        _ => return None,
    })
}
