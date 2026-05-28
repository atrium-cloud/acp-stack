//! `auth_failures` table persistence.

use crate::error::{Result, StackError};
use rusqlite::{Connection, params};

use super::core::StateStore;
use super::ids::{current_timestamp, next_auth_failure_id};
use super::records::LogOrder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFailure {
    pub id: String,
    pub created_at: String,
    pub key_kind: String,
    pub reason: String,
    pub client_ip: Option<String>,
    pub route: Option<String>,
    pub payload_json: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AuthFailureFilter<'a> {
    pub limit: u32,
    pub after_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub order: LogOrder,
}

pub(super) fn row_to_auth_failure(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuthFailure> {
    Ok(AuthFailure {
        id: row.get(0)?,
        created_at: row.get(1)?,
        key_kind: row.get(2)?,
        reason: row.get(3)?,
        client_ip: row.get(4)?,
        route: row.get(5)?,
        payload_json: row.get(6)?,
    })
}

fn validate_auth_failure_payload(connection: &Connection, payload_json: &str) -> Result<()> {
    let is_valid: i64 =
        connection.query_row("SELECT json_valid(?1)", params![payload_json], |row| {
            row.get(0)
        })?;
    if is_valid == 1 {
        return Ok(());
    }

    Err(StackError::InvalidAuthFailurePayload)
}

impl StateStore {
    pub fn append_auth_failure(
        &self,
        key_kind: &str,
        reason: &str,
        client_ip: Option<&str>,
        route: Option<&str>,
        payload_json: &str,
    ) -> Result<AuthFailure> {
        validate_auth_failure_payload(self.connection(), payload_json)?;
        let failure = AuthFailure {
            id: next_auth_failure_id(),
            created_at: current_timestamp(),
            key_kind: key_kind.to_owned(),
            reason: reason.to_owned(),
            client_ip: client_ip.map(str::to_owned),
            route: route.map(str::to_owned),
            payload_json: payload_json.to_owned(),
        };

        self.persist_with_outbox("auth_failures", &failure.id, &failure.created_at, |conn| {
            conn.execute(
                r#"
                INSERT INTO auth_failures
                    (id, created_at, key_kind, reason, client_ip, route, payload_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    failure.id,
                    failure.created_at,
                    failure.key_kind,
                    failure.reason,
                    failure.client_ip,
                    failure.route,
                    failure.payload_json,
                ],
            )?;
            Ok(())
        })?;

        Ok(failure)
    }

    pub fn query_auth_failures(&self, filter: AuthFailureFilter<'_>) -> Result<Vec<AuthFailure>> {
        let mut sql = String::from(
            "SELECT id, created_at, key_kind, reason, client_ip, route, payload_json \
             FROM auth_failures WHERE 1=1",
        );
        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filter.since {
            sql.push_str(" AND created_at >= ?");
            bindings.push(rusqlite::types::Value::Text(since.to_owned()));
        }
        if let Some(until) = filter.until {
            sql.push_str(" AND created_at < ?");
            bindings.push(rusqlite::types::Value::Text(until.to_owned()));
        }
        if let Some(after) = filter.after_id {
            match filter.order {
                LogOrder::Desc => sql.push_str(
                    " AND (created_at, id) < (SELECT created_at, id FROM auth_failures WHERE id = ?)",
                ),
                LogOrder::Asc => sql.push_str(
                    " AND (created_at, id) > (SELECT created_at, id FROM auth_failures WHERE id = ?)",
                ),
            }
            bindings.push(rusqlite::types::Value::Text(after.to_owned()));
        }
        let direction = filter.order.sql_keyword();
        sql.push_str(&format!(
            " ORDER BY created_at {direction}, id {direction} LIMIT ?"
        ));
        bindings.push(rusqlite::types::Value::Integer(i64::from(filter.limit)));
        let mut statement = self.connection().prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(bindings.iter()),
            row_to_auth_failure,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_auth_failures_since(&self, since: &str) -> Result<i64> {
        Ok(self.connection().query_row(
            "SELECT COUNT(*) FROM auth_failures WHERE created_at >= ?1",
            params![since],
            |row| row.get(0),
        )?)
    }
}
