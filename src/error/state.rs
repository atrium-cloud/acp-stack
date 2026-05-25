//! State-database and migration error helpers (`state.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        State(_) => "state.error",
        IncompatibleStateSchema { .. } => "state.incompatible_schema",
        UnmanagedStateTable { .. } => "state.unmanaged_table",
        MigrationManifestParse(_) => "state.migration_manifest_invalid",
        InvalidManifestOrder { .. } => "state.invalid_manifest_order",
        ManifestRegistryMismatch { .. } => "state.manifest_registry_mismatch",
        MissingMigratedTable { .. } => "state.missing_migrated_table",
        InvalidEventPayload => "state.invalid_event_payload",
        InvalidAuthFailurePayload => "state.invalid_auth_failure_payload",
        StateInvalidJson { .. } => "state.invalid_json",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        State(_) => "state database error".to_owned(),
        IncompatibleStateSchema { .. } => {
            "state schema is newer than this binary supports".to_owned()
        }
        UnmanagedStateTable { table } => {
            format!("existing state table `{table}` is not managed by a recorded migration")
        }
        MigrationManifestParse(_) => "migration manifest is invalid".to_owned(),
        InvalidManifestOrder { .. } => {
            "migration manifest ids must be strictly increasing positive integers".to_owned()
        }
        ManifestRegistryMismatch { .. } => {
            "migration manifest does not match the compiled registry".to_owned()
        }
        MissingMigratedTable { table } => {
            format!("state database is missing required table `{table}`")
        }
        InvalidEventPayload => "event payload must be valid JSON text".to_owned(),
        InvalidAuthFailurePayload => "auth failure payload must be valid JSON text".to_owned(),
        StateInvalidJson { field, .. } => format!("durable JSON corruption in `{field}`"),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        InvalidEventPayload | InvalidAuthFailurePayload => StatusCode::BAD_REQUEST,
        State(_)
        | IncompatibleStateSchema { .. }
        | UnmanagedStateTable { .. }
        | MigrationManifestParse(_)
        | InvalidManifestOrder { .. }
        | ManifestRegistryMismatch { .. }
        | MissingMigratedTable { .. }
        | StateInvalidJson { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
