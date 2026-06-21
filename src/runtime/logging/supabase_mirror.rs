use serde_json::{Map, Value, json};

use crate::config::SupabaseLoggingConfig;
use crate::error::{Result, StackError};

pub const SUPABASE_DEFAULT_SCHEMA: &str = "public";
pub const SUPABASE_DEFAULT_TABLE_PREFIX: &str = "acp_stack_";
pub const SUPABASE_DEFAULT_DB_URL_REF: &str = "SUPABASE_LOG_DB_URL";
pub const SUPABASE_WRITER_ROLE: &str = "acp_stack_logger";
const SUPABASE_INGEST_FUNCTION_SUFFIX: &str = "ingest_batch";
const SUPABASE_MIGRATIONS_TABLE: &str = "schema_migrations";
const SUPABASE_PUBLIC_API_ROLES: &[&str] = &["anon", "authenticated"];

pub const MIRRORED_TABLES: &[&str] = &[
    "events",
    "sessions",
    "prompts",
    "commands",
    "permission_requests",
    "permission_decisions",
    "auth_failures",
    "agent_lifecycle",
];

const SUPABASE_ANALYTICS_VIEWS: &[&str] = &[
    "session_turns",
    "permissions",
    "agent_events",
    "security_events",
    "connection_events",
    "usage_metrics",
];

pub fn remote_table_name(config: &SupabaseLoggingConfig, source_table: &str) -> Result<String> {
    if !MIRRORED_TABLES.contains(&source_table) {
        return Err(StackError::SupabaseSinkUnknownTable {
            table: source_table.to_owned(),
        });
    }
    Ok(format!("{}{}", config.table_prefix, source_table))
}

pub fn canary_event() -> Map<String, Value> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let id = format!("supabase_check_{}", now.replace([':', '.', '-'], "_"));
    let mut row = Map::new();
    row.insert("id".to_owned(), Value::String(id));
    row.insert("created_at".to_owned(), Value::String(now));
    row.insert("level".to_owned(), Value::String("info".to_owned()));
    row.insert("source".to_owned(), Value::String("cli".to_owned()));
    row.insert(
        "kind".to_owned(),
        Value::String("logging.supabase.check".to_owned()),
    );
    row.insert(
        "message".to_owned(),
        Value::String("[redacted; 0 bytes]".to_owned()),
    );
    row.insert(
        "payload_json".to_owned(),
        json!({
            "kind": "logging.supabase.check",
            "canary": true,
        }),
    );
    row
}

pub fn setup_sql(schema: &str, table_prefix: &str, writer_password: &str) -> String {
    let schema = quote_ident(schema);
    let role = quote_ident(SUPABASE_WRITER_ROLE);
    let password = quote_literal(writer_password);
    let events = table(schema.as_str(), table_prefix, "events");
    let sessions = table(schema.as_str(), table_prefix, "sessions");
    let prompts = table(schema.as_str(), table_prefix, "prompts");
    let commands = table(schema.as_str(), table_prefix, "commands");
    let permission_requests = table(schema.as_str(), table_prefix, "permission_requests");
    let permission_decisions = table(schema.as_str(), table_prefix, "permission_decisions");
    let auth_failures = table(schema.as_str(), table_prefix, "auth_failures");
    let agent_lifecycle = table(schema.as_str(), table_prefix, "agent_lifecycle");
    let migrations = table(schema.as_str(), table_prefix, SUPABASE_MIGRATIONS_TABLE);
    let session_turns = table(schema.as_str(), table_prefix, "session_turns");
    let permissions = table(schema.as_str(), table_prefix, "permissions");
    let agent_events = table(schema.as_str(), table_prefix, "agent_events");
    let security_events = table(schema.as_str(), table_prefix, "security_events");
    let connection_events = table(schema.as_str(), table_prefix, "connection_events");
    let usage_metrics = table(schema.as_str(), table_prefix, "usage_metrics");
    let ingest_function = function(
        schema.as_str(),
        table_prefix,
        SUPABASE_INGEST_FUNCTION_SUFFIX,
    );
    let ingest_function_signature = format!("{ingest_function}(text, jsonb)");
    let base_table_names = setup_base_table_names()
        .map(|name| table(schema.as_str(), table_prefix, name))
        .collect::<Vec<_>>()
        .join(", ");
    let view_names = SUPABASE_ANALYTICS_VIEWS
        .iter()
        .map(|name| table(schema.as_str(), table_prefix, name))
        .collect::<Vec<_>>()
        .join(", ");
    let rls_enables = setup_base_table_names()
        .map(|name| enable_rls_sql(schema.as_str(), table_prefix, name))
        .collect::<Vec<_>>()
        .join("\n");
    // Lock the Supabase REST API roles out of every mirrored relation. PUBLIC is
    // revoked unconditionally; `anon`/`authenticated` are revoked only when they
    // exist so the same SQL stays runnable against a non-Supabase Postgres.
    let revoke_base_tables = revoke_api_roles_sql("TABLE", &base_table_names);
    let revoke_views = revoke_api_roles_sql("TABLE", &view_names);
    let revoke_function = revoke_api_roles_sql("FUNCTION", &ingest_function_signature);
    let ingest_cases = MIRRORED_TABLES
        .iter()
        .map(|name| ingest_case_sql(schema.as_str(), table_prefix, name))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"
CREATE TABLE IF NOT EXISTS {migrations} (
    version bigint PRIMARY KEY,
    name text NOT NULL,
    applied_at timestamptz NOT NULL
);

CREATE TABLE IF NOT EXISTS {events} (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    level text NOT NULL,
    kind text NOT NULL,
    message text NOT NULL,
    payload_json jsonb NOT NULL,
    session_id text,
    source text NOT NULL DEFAULT 'system'
);

CREATE TABLE IF NOT EXISTS {sessions} (
    id text PRIMARY KEY,
    target_id text NOT NULL DEFAULT '',
    agent_session_id text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    status text NOT NULL,
    agent_id text NOT NULL DEFAULT '',
    cwd text NOT NULL DEFAULT '',
    title text,
    metadata_json jsonb NOT NULL DEFAULT '{{}}'::jsonb
);

CREATE TABLE IF NOT EXISTS {prompts} (
    id text PRIMARY KEY,
    session_id text NOT NULL,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    status text NOT NULL,
    stop_reason text,
    error_code text,
    error_message text,
    prompt_json jsonb NOT NULL,
    failure_class text,
    failure_detail_json jsonb,
    message_id text,
    message_id_acknowledged boolean NOT NULL DEFAULT false
);

CREATE TABLE IF NOT EXISTS {commands} (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    status text NOT NULL,
    command text NOT NULL,
    exit_status bigint,
    started_at timestamptz,
    finished_at timestamptz,
    cwd text,
    env_json jsonb,
    duration_ms bigint,
    truncated bigint NOT NULL DEFAULT 0,
    last_output_event_id text,
    last_output_at timestamptz,
    last_output_seq bigint,
    output_bytes bigint NOT NULL DEFAULT 0,
    last_progress_at timestamptz
);

CREATE TABLE IF NOT EXISTS {permission_requests} (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    status text NOT NULL,
    source text NOT NULL,
    requester text,
    subject_id text,
    detail_json jsonb NOT NULL,
    expires_at timestamptz
);

CREATE TABLE IF NOT EXISTS {permission_decisions} (
    id text PRIMARY KEY,
    request_id text NOT NULL,
    created_at timestamptz NOT NULL,
    decision text NOT NULL,
    deciding_principal text,
    reason text
);

CREATE TABLE IF NOT EXISTS {auth_failures} (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    key_kind text NOT NULL,
    reason text NOT NULL,
    client_ip text,
    route text,
    payload_json jsonb NOT NULL
);

CREATE TABLE IF NOT EXISTS {agent_lifecycle} (
    id text PRIMARY KEY,
    created_at timestamptz NOT NULL,
    event_kind text NOT NULL,
    message text NOT NULL,
    payload_json jsonb NOT NULL
);

{rls_enables}

{revoke_base_tables}

CREATE INDEX IF NOT EXISTS {idx_events_source} ON {events}(source);
CREATE INDEX IF NOT EXISTS {idx_events_created_kind} ON {events}(created_at, kind);
CREATE INDEX IF NOT EXISTS {idx_events_kind_created} ON {events}(kind, created_at);
CREATE INDEX IF NOT EXISTS {idx_sessions_updated} ON {sessions}(updated_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS {idx_prompts_session} ON {prompts}(session_id, created_at, id);
CREATE INDEX IF NOT EXISTS {idx_prompts_status} ON {prompts}(status, updated_at);
CREATE INDEX IF NOT EXISTS {idx_prompts_created} ON {prompts}(created_at, session_id, id);
CREATE INDEX IF NOT EXISTS {idx_prompts_updated} ON {prompts}(updated_at, session_id, id);
CREATE INDEX IF NOT EXISTS {idx_commands_progress} ON {commands}(status, last_progress_at);
CREATE INDEX IF NOT EXISTS {idx_permission_requests_status} ON {permission_requests}(status, created_at);
CREATE INDEX IF NOT EXISTS {idx_permission_decisions_request} ON {permission_decisions}(request_id);

CREATE OR REPLACE VIEW {session_turns}
WITH (security_invoker = true) AS
SELECT id, session_id, status, stop_reason, error_code, error_message,
       created_at, updated_at, prompt_json
FROM {prompts};

CREATE OR REPLACE VIEW {permissions}
WITH (security_invoker = true) AS
SELECT
    r.id AS request_id,
    r.created_at AS requested_at,
    r.updated_at AS request_updated_at,
    r.status,
    r.source,
    r.requester,
    r.subject_id,
    r.detail_json,
    r.expires_at,
    d.id AS decision_id,
    d.created_at AS decided_at,
    d.decision,
    d.deciding_principal,
    d.reason
FROM {permission_requests} AS r
LEFT JOIN {permission_decisions} AS d ON d.request_id = r.id;

CREATE OR REPLACE VIEW {agent_events}
WITH (security_invoker = true) AS
SELECT id, created_at, event_kind AS kind, message, payload_json,
       'agent_lifecycle'::text AS source
FROM {agent_lifecycle}
UNION ALL
SELECT id, created_at, kind, message, payload_json, source
FROM {events}
WHERE kind LIKE 'agent.%';

CREATE OR REPLACE VIEW {security_events}
WITH (security_invoker = true) AS
SELECT id, created_at, key_kind AS kind, reason AS message, payload_json,
       'auth_failures'::text AS source
FROM {auth_failures}
UNION ALL
SELECT id, created_at, kind, message, payload_json, source
FROM {events}
WHERE kind LIKE 'security.%';

CREATE OR REPLACE VIEW {connection_events}
WITH (security_invoker = true) AS
SELECT id, created_at, kind, message, payload_json, source, session_id
FROM {events}
WHERE kind IN ('api.request', 'ws.client_connected', 'ws.client_disconnected');

CREATE OR REPLACE VIEW {usage_metrics}
WITH (security_invoker = true) AS
SELECT id, created_at, kind, message, payload_json, source, session_id
FROM {events}
WHERE kind = 'usage.reported';

{revoke_views}

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{writer_role_raw}') THEN
        CREATE ROLE {role} LOGIN PASSWORD {password};
    ELSE
        ALTER ROLE {role} WITH LOGIN PASSWORD {password};
    END IF;
END $$;

GRANT USAGE ON SCHEMA {schema} TO {role};
REVOKE ALL ON TABLE {base_table_names} FROM {role};
REVOKE ALL ON TABLE {view_names} FROM {role};

CREATE OR REPLACE FUNCTION {ingest_function}(source_table text, payload jsonb)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = {schema}, pg_temp
AS $function$
BEGIN
    CASE source_table
{ingest_cases}
    ELSE
        RAISE EXCEPTION 'unsupported acp-stack mirror table: %', source_table
            USING ERRCODE = 'invalid_parameter_value';
    END CASE;
END
$function$;

{revoke_function}
GRANT EXECUTE ON FUNCTION {ingest_function_signature} TO {role};

INSERT INTO {migrations} (version, name, applied_at)
VALUES (1, 'supabase_cli_backed_logging_setup', now())
ON CONFLICT (version) DO UPDATE SET
    name = excluded.name,
    applied_at = excluded.applied_at;
"#,
        idx_events_source = index_name(table_prefix, "events_source_idx"),
        idx_events_created_kind = index_name(table_prefix, "events_created_kind_idx"),
        idx_events_kind_created = index_name(table_prefix, "events_kind_created_idx"),
        idx_sessions_updated = index_name(table_prefix, "sessions_updated_at_idx"),
        idx_prompts_session = index_name(table_prefix, "prompts_session_idx"),
        idx_prompts_status = index_name(table_prefix, "prompts_status_updated_at_idx"),
        idx_prompts_created = index_name(table_prefix, "prompts_created_at_idx"),
        idx_prompts_updated = index_name(table_prefix, "prompts_updated_at_idx"),
        idx_commands_progress = index_name(table_prefix, "commands_last_progress_idx"),
        idx_permission_requests_status =
            index_name(table_prefix, "permission_requests_status_created_idx"),
        idx_permission_decisions_request =
            index_name(table_prefix, "permission_decisions_request_idx"),
        writer_role_raw = SUPABASE_WRITER_ROLE,
    )
}

pub fn check_table_sql(config: &SupabaseLoggingConfig, source_table: &str) -> Result<String> {
    let remote = remote_table_name(config, source_table)?;
    Ok(format!(
        "SELECT to_regclass('{}') IS NOT NULL",
        qualified_regclass(&config.schema, &remote)
    ))
}

pub fn postgres_insert_sql(config: &SupabaseLoggingConfig, source_table: &str) -> Result<String> {
    if !MIRRORED_TABLES.contains(&source_table) {
        return Err(StackError::SupabaseSinkUnknownTable {
            table: source_table.to_owned(),
        });
    }
    let ingest_function = format!(
        "{}.{}",
        quote_ident(&config.schema),
        quote_ident(&format!(
            "{}{SUPABASE_INGEST_FUNCTION_SUFFIX}",
            config.table_prefix
        ))
    );
    Ok(format!("SELECT {ingest_function}($1::text, $2::jsonb)"))
}

fn postgres_upsert_sql(
    quoted_schema: &str,
    table_prefix: &str,
    source_table: &str,
    payload_expression: &str,
) -> Result<String> {
    let target = table(quoted_schema, table_prefix, source_table);
    let assignments = columns_for(source_table)?
        .iter()
        .copied()
        .filter(|column| *column != "id")
        .map(|column| {
            let quoted = quote_ident(column);
            format!("{quoted} = EXCLUDED.{quoted}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "INSERT INTO {target} SELECT * FROM jsonb_populate_recordset(NULL::{target}, {payload_expression}) \
         ON CONFLICT (id) DO UPDATE SET {assignments}"
    ))
}

fn columns_for(source_table: &str) -> Result<&'static [&'static str]> {
    match source_table {
        "events" => Ok(&[
            "id",
            "created_at",
            "level",
            "kind",
            "message",
            "payload_json",
            "session_id",
            "source",
        ]),
        "sessions" => Ok(&[
            "id",
            "target_id",
            "agent_session_id",
            "created_at",
            "updated_at",
            "status",
            "agent_id",
            "cwd",
            "title",
            "metadata_json",
        ]),
        "prompts" => Ok(&[
            "id",
            "session_id",
            "created_at",
            "updated_at",
            "status",
            "stop_reason",
            "error_code",
            "error_message",
            "prompt_json",
            "failure_class",
            "failure_detail_json",
            "message_id",
            "message_id_acknowledged",
        ]),
        "commands" => Ok(&[
            "id",
            "created_at",
            "updated_at",
            "status",
            "command",
            "exit_status",
            "started_at",
            "finished_at",
            "cwd",
            "env_json",
            "duration_ms",
            "truncated",
            "last_output_event_id",
            "last_output_at",
            "last_output_seq",
            "output_bytes",
            "last_progress_at",
        ]),
        "permission_requests" => Ok(&[
            "id",
            "created_at",
            "updated_at",
            "status",
            "source",
            "requester",
            "subject_id",
            "detail_json",
            "expires_at",
        ]),
        "permission_decisions" => Ok(&[
            "id",
            "request_id",
            "created_at",
            "decision",
            "deciding_principal",
            "reason",
        ]),
        "auth_failures" => Ok(&[
            "id",
            "created_at",
            "key_kind",
            "reason",
            "client_ip",
            "route",
            "payload_json",
        ]),
        "agent_lifecycle" => Ok(&["id", "created_at", "event_kind", "message", "payload_json"]),
        other => Err(StackError::SupabaseSinkUnknownTable {
            table: other.to_owned(),
        }),
    }
}

fn table(quoted_schema: &str, prefix: &str, name: &str) -> String {
    format!(
        "{quoted_schema}.{}",
        quote_ident(&format!("{prefix}{name}"))
    )
}

fn function(quoted_schema: &str, prefix: &str, name: &str) -> String {
    format!(
        "{quoted_schema}.{}",
        quote_ident(&format!("{prefix}{name}"))
    )
}

fn index_name(prefix: &str, name: &str) -> String {
    quote_ident(&format!("{prefix}{name}"))
}

fn setup_base_table_names() -> impl Iterator<Item = &'static str> {
    std::iter::once(SUPABASE_MIGRATIONS_TABLE).chain(MIRRORED_TABLES.iter().copied())
}

fn enable_rls_sql(quoted_schema: &str, prefix: &str, name: &str) -> String {
    let target = table(quoted_schema, prefix, name);
    format!("ALTER TABLE {target} ENABLE ROW LEVEL SECURITY;")
}

/// Revoke every privilege on `targets` from the Supabase REST API roles.
/// `object_kind` is the SQL object class (`TABLE` or `FUNCTION`) and `targets`
/// is the already-qualified, comma-joined relation/signature list. PUBLIC is
/// revoked unconditionally; `anon`/`authenticated` are revoked inside a
/// `pg_roles` guard so the statement also succeeds where those roles are absent
/// (e.g. a generic Postgres reached via the printed `acps logging supabase sql`).
fn revoke_api_roles_sql(object_kind: &str, targets: &str) -> String {
    let api_role_array = SUPABASE_PUBLIC_API_ROLES
        .iter()
        .map(|role| quote_literal(role))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"REVOKE ALL ON {object_kind} {targets} FROM PUBLIC;

DO $$
DECLARE
    api_role_name text;
BEGIN
    FOREACH api_role_name IN ARRAY ARRAY[{api_role_array}] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name) THEN
            EXECUTE format('REVOKE ALL ON {object_kind} {targets} FROM %I', api_role_name);
        END IF;
    END LOOP;
END $$;"#
    )
}

fn ingest_case_sql(quoted_schema: &str, prefix: &str, name: &str) -> String {
    let upsert = postgres_upsert_sql(quoted_schema, prefix, name, "payload")
        .expect("ingest cases are generated only for mirrored tables");
    format!(
        r#"    WHEN '{name}' THEN
        {upsert};"#
    )
}

fn qualified_regclass(schema: &str, table: &str) -> String {
    format!(
        "{}.{}",
        escape_regclass_part(schema),
        escape_regclass_part(table)
    )
}

fn escape_regclass_part(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated_sql() -> String {
        setup_sql("public", "acp_stack_", "test_writer_password")
    }

    fn generated_relation(name: &str) -> String {
        format!("\"public\".\"acp_stack_{name}\"")
    }

    #[test]
    fn setup_sql_enables_rls_on_every_created_base_table() {
        let sql = generated_sql();

        for table_name in setup_base_table_names() {
            let relation = generated_relation(table_name);
            assert!(
                sql.contains(&format!(
                    "ALTER TABLE {relation} ENABLE ROW LEVEL SECURITY;"
                )),
                "missing RLS enablement for {relation}"
            );
        }
    }

    #[test]
    fn setup_sql_revokes_public_api_roles_from_tables_and_views() {
        let sql = generated_sql();
        let table_names = setup_base_table_names()
            .map(generated_relation)
            .collect::<Vec<_>>()
            .join(", ");
        let view_names = SUPABASE_ANALYTICS_VIEWS
            .iter()
            .map(|name| generated_relation(name))
            .collect::<Vec<_>>()
            .join(", ");

        // PUBLIC is revoked unconditionally; the API roles are revoked only via a
        // pg_roles existence guard so the SQL is safe on a non-Supabase Postgres.
        for targets in [&table_names, &view_names] {
            assert!(
                sql.contains(&format!("REVOKE ALL ON TABLE {targets} FROM PUBLIC;")),
                "missing unconditional PUBLIC revoke for {targets}"
            );
            assert!(
                sql.contains(&format!(
                    "EXECUTE format('REVOKE ALL ON TABLE {targets} FROM %I', api_role_name);"
                )),
                "missing guarded API-role revoke for {targets}"
            );
        }
        assert!(
            sql.contains("IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = api_role_name)"),
            "API-role revoke must be guarded by a pg_roles existence check"
        );
        for role in ["\"anon\"", "\"authenticated\""] {
            assert!(
                !sql.contains(&format!("FROM PUBLIC, {role}")),
                "API role {role} must not be revoked unconditionally"
            );
        }

        for role in ["PUBLIC", "\"anon\"", "\"authenticated\""] {
            assert!(
                !sql.contains(&format!(" TO {role}")),
                "generated SQL must not grant privileges to {role}"
            );
        }
    }

    #[test]
    fn setup_sql_uses_security_invoker_views() {
        let sql = generated_sql();

        for view_name in SUPABASE_ANALYTICS_VIEWS {
            let relation = generated_relation(view_name);
            assert!(
                sql.contains(&format!(
                    "CREATE OR REPLACE VIEW {relation}\nWITH (security_invoker = true) AS"
                )),
                "missing security_invoker for {relation}"
            );
        }
    }
}
