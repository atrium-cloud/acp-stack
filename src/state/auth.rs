//! Auth verifier and auth-failure table persistence.

use crate::auth::{AuthVerifier, KeyKind};
use crate::error::{Result, StackError};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthKeyRecord {
    pub key_kind: String,
    pub algorithm: String,
    pub salt: String,
    pub digest: String,
    pub created_at: String,
    pub updated_at: String,
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

fn row_to_auth_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuthKeyRecord> {
    Ok(AuthKeyRecord {
        key_kind: row.get(0)?,
        algorithm: row.get(1)?,
        salt: row.get(2)?,
        digest: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
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
    pub fn get_auth_key(&self, key_kind: KeyKind) -> Result<Option<AuthKeyRecord>> {
        let mut statement = self.connection().prepare(
            "SELECT key_kind, algorithm, salt, digest, created_at, updated_at \
             FROM auth_keys WHERE key_kind = ?1",
        )?;
        let mut rows = statement.query(params![key_kind.as_wire_str()])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_auth_key(row)?)),
            None => Ok(None),
        }
    }

    pub fn auth_key_pair_present(&self) -> Result<bool> {
        let session_present = self.get_auth_key(KeyKind::Session)?.is_some();
        let admin_present = self.get_auth_key(KeyKind::Admin)?.is_some();
        if session_present && admin_present {
            self.load_auth_verifier_pair()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn load_auth_verifier_pair(&self) -> Result<crate::auth::AuthVerifierSet> {
        let session = self
            .get_auth_key(KeyKind::Session)?
            .ok_or(StackError::MissingField {
                field: "auth_keys.session",
            })?;
        let admin = self
            .get_auth_key(KeyKind::Admin)?
            .ok_or(StackError::MissingField {
                field: "auth_keys.admin",
            })?;
        Ok(crate::auth::AuthVerifierSet {
            session: auth_key_record_to_verifier(KeyKind::Session, session)?,
            admin: auth_key_record_to_verifier(KeyKind::Admin, admin)?,
        })
    }

    pub fn upsert_auth_key(
        &self,
        key_kind: KeyKind,
        verifier: &AuthVerifier,
    ) -> Result<AuthKeyRecord> {
        let now = current_timestamp();
        let existing_created_at = self
            .get_auth_key(key_kind)?
            .map(|record| record.created_at)
            .unwrap_or_else(|| now.clone());
        let record = AuthKeyRecord {
            key_kind: key_kind.as_wire_str().to_owned(),
            algorithm: verifier.algorithm().to_owned(),
            salt: verifier.encoded_salt(),
            digest: verifier.encoded_digest(),
            created_at: existing_created_at,
            updated_at: now,
        };
        self.connection().execute(
            r#"
            INSERT INTO auth_keys
                (key_kind, algorithm, salt, digest, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(key_kind) DO UPDATE SET
                algorithm = excluded.algorithm,
                salt = excluded.salt,
                digest = excluded.digest,
                updated_at = excluded.updated_at
            "#,
            params![
                record.key_kind,
                record.algorithm,
                record.salt,
                record.digest,
                record.created_at,
                record.updated_at,
            ],
        )?;
        Ok(record)
    }

    pub fn insert_auth_key_pair(&self, verifiers: &crate::auth::AuthVerifierSet) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let now = current_timestamp();
        insert_auth_key_in_transaction(&transaction, KeyKind::Session, &verifiers.session, &now)?;
        insert_auth_key_in_transaction(&transaction, KeyKind::Admin, &verifiers.admin, &now)?;
        transaction.commit()?;
        Ok(())
    }

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

fn auth_key_record_to_verifier(key_kind: KeyKind, record: AuthKeyRecord) -> Result<AuthVerifier> {
    AuthVerifier::from_encoded(key_kind, record.algorithm, record.salt, record.digest)
}

fn insert_auth_key_in_transaction(
    transaction: &Transaction<'_>,
    key_kind: KeyKind,
    verifier: &AuthVerifier,
    now: &str,
) -> Result<()> {
    transaction.execute(
        r#"
        INSERT INTO auth_keys
            (key_kind, algorithm, salt, digest, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?5)
        "#,
        params![
            key_kind.as_wire_str(),
            verifier.algorithm(),
            verifier.encoded_salt(),
            verifier.encoded_digest(),
            now,
        ],
    )?;
    Ok(())
}
