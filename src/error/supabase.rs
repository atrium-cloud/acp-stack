//! Supabase logging sink and config error helpers (`logging.supabase.*`).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        MissingSupabaseApiKey { .. } => "logging.supabase.missing_api_key",
        InvalidSupabaseUrl { .. } => "logging.supabase.invalid_url",
        InvalidSupabaseSchema { .. } => "logging.supabase.invalid_schema",
        SupabaseSinkHttp { .. } => "logging.supabase.http_error",
        SupabaseSinkUnknownTable { .. } => "logging.supabase.unknown_table",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        MissingSupabaseApiKey { .. } => {
            "secret store is missing Supabase secret API key reference".to_owned()
        }
        InvalidSupabaseUrl { .. } => "[logging.supabase].url must start with `https://`".to_owned(),
        InvalidSupabaseSchema { .. } => {
            "[logging.supabase].schema is not a safe Postgres identifier".to_owned()
        }
        SupabaseSinkHttp { status, .. } => {
            format!("Supabase sink rejected upload with HTTP {status}")
        }
        SupabaseSinkUnknownTable { table } => {
            format!("Supabase sink received a row for unknown source table `{table}`")
        }
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        MissingSupabaseApiKey { .. }
        | InvalidSupabaseUrl { .. }
        | InvalidSupabaseSchema { .. }
        | SupabaseSinkHttp { .. }
        | SupabaseSinkUnknownTable { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => return None,
    })
}
