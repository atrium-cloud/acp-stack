//! Supabase logging sink worker: drains `sink_outbox`, redacts each row, and
//! POSTs batched JSON with `Prefer: resolution=merge-duplicates` so replay is
//! idempotent. Local SQLite writes never block on the sink.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use rand::RngExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::config::{SupabaseLoggingBackend, SupabaseLoggingConfig};
use crate::error::{Result, StackError};
use crate::events::EventHub;
use crate::state::{StateStore, sink_outbox::OutboxRow};

use super::sink_redaction::redact_row;
use super::supabase_mirror;

/// Sink poll cadence: doubles on empty fetches, resets on a non-empty one.
const POLL_INTERVAL_MIN: Duration = Duration::from_secs(1);
const POLL_INTERVAL_MAX: Duration = Duration::from_secs(30);
const BATCH_SIZE: usize = 128;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const POSTGRES_TIMEOUT: Duration = Duration::from_secs(15);
const SUPABASE_ROOT_2021_CA_DER_BASE64: &str = "MIIDxDCCAqygAwIBAgIUbLxMod62P2ktCiAkxnKJwtE9VPYwDQYJKoZIhvcNAQELBQAwazELMAkGA1UEBhMCVVMxEDAOBgNVBAgMB0RlbHdhcmUxEzARBgNVBAcMCk5ldyBDYXN0bGUxFTATBgNVBAoMDFN1cGFiYXNlIEluYzEeMBwGA1UEAwwVU3VwYWJhc2UgUm9vdCAyMDIxIENBMB4XDTIxMDQyODEwNTY1M1oXDTMxMDQyNjEwNTY1M1owazELMAkGA1UEBhMCVVMxEDAOBgNVBAgMB0RlbHdhcmUxEzARBgNVBAcMCk5ldyBDYXN0bGUxFTATBgNVBAoMDFN1cGFiYXNlIEluYzEeMBwGA1UEAwwVU3VwYWJhc2UgUm9vdCAyMDIxIENBMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAqQXWQyHOB+qR2GJobCq/CBmQ40G0oDmCC3mzVnn8sv4XNeWtE5XcEL0uVih7Jo4Dkx1QDmGHBH1zDfgs2qXiLb6xpw/CKQPypZW1JssOTMIfQppNQ87K75Ya0p25Y3ePS2t2GtvHxNjUV6kjOZjEn2yWEcBdpOVCUYBVFBNMB4YBHkNRDa/+S4uywAoaTWnCJLUicvTlHmMw6xSQQn1UfRQHk50DMCEJ7Cy1RxrZJrkXXRP3LqQL2ijJ6F4yMfh+Gyb4O4XajoVj/+R4GwywKYrrS8PrSNtwxr5StlQO8zIQUSMiq26wM8mgELFlS/32UcltNaQ1xBRizkzpZct9DwIDAQABo2AwXjALBgNVHQ8EBAMCAQYwHQYDVR0OBBYEFKjXuXY32CztkhImng4yJNUtaUYsMB8GA1UdIwQYMBaAFKjXuXY32CztkhImng4yJNUtaUYsMA8GA1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQELBQADggEBAB8spzNn+4VUtVxbdMaX+39Z50sc7uATmus16jmmHjhIHz+l/9GlJ5KqAMOx26mPZgfzG7oneL2bVW+WgYUkTT3XEPFWnTp2RJwQao8/tYPXWEJDc0WVQHrpmnWOFKU/d3MqBgBm5y+6jB81TU/RG2rVerPDWP+1MMcNNy0491CTL5XQZ7JfDJJ9CCmXSdtTl4uUQnSuv/QxCea13BX2ZgJc7Au30vihLhub52De4P/4gonKsNHYdbWjg7OWKwNv/zitGDVDB9Y2CMTyZKG3XEu5Ghl1LEnI3QmEKsqaCLv12BnVjbkSeZsMnevJPs1Ye6TjjJwdik5Po/bKiIz+Fq8=";
const FAILURE_WINDOW_LEN: Duration = Duration::from_secs(60);

struct UploadGroupContext<'a> {
    state: &'a Arc<TokioMutex<StateStore>>,
    client: &'a reqwest::Client,
    config: &'a SupabaseLoggingConfig,
    credential: &'a SupabaseSinkCredential,
    event_hub: &'a EventHub,
    failure_window_count: &'a mut i64,
    failure_window_last_error: &'a mut Option<String>,
}

#[derive(Debug, Clone)]
pub enum SupabaseSinkCredential {
    PostgrestApiKey(String),
    PostgresDbUrl(String),
}

/// Background worker owning the Supabase client and a shutdown latch. Call
/// `shutdown` on graceful stop so the in-flight batch finishes draining.
pub struct SupabaseSink {
    shutdown: Arc<Notify>,
    handle: Option<JoinHandle<()>>,
}

impl SupabaseSink {
    pub fn spawn(
        state: Arc<TokioMutex<StateStore>>,
        config: SupabaseLoggingConfig,
        credential: SupabaseSinkCredential,
        event_hub: EventHub,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .pool_max_idle_per_host(4)
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(|err| StackError::SupabaseSinkHttp {
                status: 0,
                body: format!("client build failed: {err}"),
            })?;
        let shutdown = Arc::new(Notify::new());
        let shutdown_for_worker = shutdown.clone();
        let handle = tokio::spawn(async move {
            run_worker(
                state,
                config,
                credential,
                client,
                event_hub,
                shutdown_for_worker,
            )
            .await;
        });
        Ok(Self {
            shutdown,
            handle: Some(handle),
        })
    }

    pub async fn shutdown(mut self) {
        // `notify_one` stores a permit; `notify_waiters` would drop a signal
        // raised between loop iterations, since the latch is only checked
        // inside the worker's select!.
        self.shutdown.notify_one();
        if let Some(handle) = self.handle.take()
            && let Err(err) = handle.await
            && !err.is_cancelled()
        {
            tracing::warn!(error = %err, "supabase sink worker panicked during shutdown");
        }
    }
}

impl Drop for SupabaseSink {
    fn drop(&mut self) {
        if self.handle.is_some() {
            // Signal without blocking: Drop can run from a sync context, but
            // the task must not outlive the daemon's tokio runtime.
            self.shutdown.notify_one();
        }
    }
}

async fn run_worker(
    state: Arc<TokioMutex<StateStore>>,
    config: SupabaseLoggingConfig,
    credential: SupabaseSinkCredential,
    client: reqwest::Client,
    event_hub: EventHub,
    shutdown: Arc<Notify>,
) {
    let mut poll_interval = POLL_INTERVAL_MIN;
    let mut failure_window_start = current_timestamp();
    let mut failure_window_count: i64 = 0;
    let mut failure_window_last_error: Option<String> = None;
    let mut window_started_at = std::time::Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => {
                tracing::info!("supabase sink worker received shutdown signal");
                break;
            }
            _ = sleep(poll_interval) => {}
        }

        if let Some(processed) = process_one_batch(
            &state,
            &client,
            &config,
            &credential,
            &event_hub,
            &mut failure_window_count,
            &mut failure_window_last_error,
        )
        .await
        {
            poll_interval = if processed {
                POLL_INTERVAL_MIN
            } else {
                next_backoff_interval(poll_interval)
            };
        } else {
            poll_interval = next_backoff_interval(poll_interval);
        }

        if window_started_at.elapsed() >= FAILURE_WINDOW_LEN {
            roll_failure_window(
                &state,
                &mut failure_window_start,
                &mut failure_window_count,
                &mut failure_window_last_error,
                &mut window_started_at,
            )
            .await;
        }
    }

    // The daemon writes `agent.stopped`/`server.stopped` AFTER signaling sink
    // shutdown, so drain again here. Bounded so an unrecoverable 5xx cannot
    // block daemon exit.
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < drain_deadline {
        match process_one_batch(
            &state,
            &client,
            &config,
            &credential,
            &event_hub,
            &mut failure_window_count,
            &mut failure_window_last_error,
        )
        .await
        {
            Some(true) => {}
            Some(false) => break,
            None => break,
        }
    }

    // Flush so failures from the last partial minute land in the summary.
    if failure_window_count > 0 {
        let now = current_timestamp();
        if let Err(err) = state.lock().await.record_sink_failure_window(
            &failure_window_start,
            failure_window_count,
            failure_window_last_error.as_deref().unwrap_or(""),
            &now,
        ) {
            tracing::warn!(error = %err, "failed to flush final sink failure summary");
        }
    }
}

/// Run one outbox poll cycle: `Some(true)` shipped rows, `Some(false)` empty
/// outbox, `None` read failure (caller backs off).
async fn process_one_batch(
    state: &Arc<TokioMutex<StateStore>>,
    client: &reqwest::Client,
    config: &SupabaseLoggingConfig,
    credential: &SupabaseSinkCredential,
    event_hub: &EventHub,
    failure_window_count: &mut i64,
    failure_window_last_error: &mut Option<String>,
) -> Option<bool> {
    let now = current_timestamp();
    let batch_result = {
        let guard = state.lock().await;
        // The worker is strictly sequential, so anything still `sending`
        // between polls was stranded by a crashed worker or a failed mark_*
        // write, and must be released before claiming the next batch.
        match guard.recover_sink_outbox_in_flight() {
            Ok(0) => {}
            Ok(recovered) => {
                tracing::warn!(
                    rows = recovered,
                    "recovered sink outbox rows stranded by an interrupted upload"
                );
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to recover in-flight sink outbox rows");
            }
        }
        guard.next_sink_outbox_batch(BATCH_SIZE, &now)
    };
    let batch = match batch_result {
        Ok(b) => b,
        Err(err) => {
            tracing::error!(error = %err, "supabase sink failed to read outbox batch");
            return None;
        }
    };
    if batch.is_empty() {
        return Some(false);
    }

    let mut groups: std::collections::BTreeMap<String, Vec<OutboxRow>> =
        std::collections::BTreeMap::new();
    for row in batch {
        groups
            .entry(row.source_table.clone())
            .or_default()
            .push(row);
    }

    for (table, rows) in groups {
        let mut context = UploadGroupContext {
            state,
            client,
            config,
            credential,
            event_hub,
            failure_window_count,
            failure_window_last_error,
        };
        match upload_group(&mut context, &table, &rows).await {
            Ok(()) => {}
            Err(err) => {
                let error_text = err.to_string();
                record_failure_observation(
                    rows.len(),
                    error_text.clone(),
                    failure_window_count,
                    failure_window_last_error,
                );
                tracing::warn!(
                    table = %table,
                    rows = rows.len(),
                    error = %error_text,
                    "supabase sink upload failed; backing off"
                );
            }
        }
    }
    Some(true)
}

async fn upload_group(
    context: &mut UploadGroupContext<'_>,
    table: &str,
    rows: &[OutboxRow],
) -> Result<()> {
    // Hydrate and redact before any I/O so the state mutex is held briefly.
    let mut payloads: Vec<Value> = Vec::with_capacity(rows.len());
    let mut acked_ids: Vec<String> = Vec::new();
    let mut payload_ids: Vec<String> = Vec::new();
    let mut hydration_failed: Vec<(String, String)> = Vec::new();
    {
        let guard = context.state.lock().await;
        for row in rows {
            match guard.hydrate_sink_outbox_row(table, &row.source_id) {
                Ok(Some(mut hydrated)) => match redact_row(table, &mut hydrated) {
                    Ok(()) => {
                        payloads.push(Value::Object(hydrated));
                        payload_ids.push(row.id.clone());
                    }
                    Err(err) => hydration_failed.push((row.id.clone(), err.to_string())),
                },
                Ok(None) => acked_ids.push(row.id.clone()),
                Err(err) => hydration_failed.push((row.id.clone(), err.to_string())),
            }
        }
    }

    if !hydration_failed.is_empty() {
        let now = current_timestamp();
        // Hydration/redaction errors are deterministic, so park them 24h out
        // rather than hot-looping until an operator fixes the drift.
        let next_attempt = offset_iso(&now, Duration::from_secs(24 * 3600));
        let ids: Vec<String> = hydration_failed.iter().map(|(id, _)| id.clone()).collect();
        let aggregated_error = hydration_failed
            .iter()
            .map(|(_, err)| err.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let guard = context.state.lock().await;
        guard.mark_sink_outbox_failure(&ids, &aggregated_error, &next_attempt, &now)?;
        drop(guard);
        record_failure_observation(
            ids.len(),
            aggregated_error.clone(),
            context.failure_window_count,
            context.failure_window_last_error,
        );
        context.event_hub.publish(crate::events::LiveEvent {
            event_type: "event",
            id: format!("sink_{table}_hydration_{now}"),
            topic: "sink".to_owned(),
            created_at: now.clone(),
            payload: serde_json::json!({
                "kind": "sink.delivery.hydration_failed",
                "data": {
                    "table": table,
                    "count": ids.len(),
                    "error": aggregated_error,
                },
            }),
        });
    }

    // Ack deleted-source rows regardless of the HTTP outcome, so they do not
    // get stuck riding along with a failing batch.
    if !acked_ids.is_empty() {
        let now = current_timestamp();
        context
            .state
            .lock()
            .await
            .mark_sink_outbox_sent(&acked_ids, &now)?;
    }

    let send_result = if payloads.is_empty() {
        Ok(())
    } else {
        send_batch(
            context.client,
            context.config,
            context.credential,
            table,
            &payloads,
        )
        .await
    };

    let now = current_timestamp();

    match send_result {
        Ok(()) => {
            if !payload_ids.is_empty() {
                context
                    .state
                    .lock()
                    .await
                    .mark_sink_outbox_sent(&payload_ids, &now)?;
            }
            context.event_hub.publish(crate::events::LiveEvent {
                event_type: "event",
                id: format!("sink_{table}_{now}"),
                topic: "sink".to_owned(),
                created_at: now.clone(),
                payload: serde_json::json!({
                    "kind": "sink.delivery.batch_sent",
                    "data": {
                        "table": table,
                        "count": payload_ids.len() + acked_ids.len(),
                    },
                }),
            });
            Ok(())
        }
        Err(err) => {
            let (permanent, message) = classify_error(&err);
            let next_attempt_at = if permanent {
                offset_iso(&now, Duration::from_secs(24 * 3600))
            } else {
                let max_attempt = rows.iter().map(|r| r.attempts).max().unwrap_or(0);
                let delay = backoff_delay(max_attempt + 1);
                offset_iso(&now, delay)
            };
            if !payload_ids.is_empty() {
                context.state.lock().await.mark_sink_outbox_failure(
                    &payload_ids,
                    &message,
                    &next_attempt_at,
                    &now,
                )?;
            }
            context.event_hub.publish(crate::events::LiveEvent {
                event_type: "event",
                id: format!("sink_{table}_{now}"),
                topic: "sink".to_owned(),
                created_at: now.clone(),
                payload: serde_json::json!({
                    "kind": "sink.delivery.batch_failed",
                    "data": {
                        "table": table,
                        "count": payload_ids.len(),
                        "permanent": permanent,
                        "next_attempt_at": next_attempt_at,
                        "error": message,
                    },
                }),
            });
            Err(err)
        }
    }
}

async fn send_batch(
    client: &reqwest::Client,
    config: &SupabaseLoggingConfig,
    credential: &SupabaseSinkCredential,
    table: &str,
    payloads: &[Value],
) -> Result<()> {
    match config.backend {
        SupabaseLoggingBackend::Postgrest => {
            let SupabaseSinkCredential::PostgrestApiKey(api_key) = credential else {
                return Err(StackError::SupabaseSinkHttp {
                    status: 0,
                    body: "postgrest backend received non-PostgREST credential".to_owned(),
                });
            };
            send_postgrest_batch(client, config, api_key, table, payloads).await
        }
        SupabaseLoggingBackend::Postgres => {
            let SupabaseSinkCredential::PostgresDbUrl(db_url) = credential else {
                return Err(StackError::SupabaseSinkHttp {
                    status: 0,
                    body: "postgres backend received non-Postgres credential".to_owned(),
                });
            };
            send_postgres_batch(config, db_url, table, payloads).await
        }
    }
}

async fn send_postgrest_batch(
    client: &reqwest::Client,
    config: &SupabaseLoggingConfig,
    api_key: &str,
    table: &str,
    payloads: &[Value],
) -> Result<()> {
    let remote_table = supabase_mirror::remote_table_name(config, table)?;
    let url = format!(
        "{}/rest/v1/{remote_table}",
        config.url.trim_end_matches('/'),
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("apikey"),
        HeaderValue::from_str(api_key).map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("invalid API key header: {err}"),
        })?,
    );
    headers.insert(
        HeaderName::from_static("content-profile"),
        HeaderValue::from_str(&config.schema).map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("invalid content-profile header: {err}"),
        })?,
    );
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("prefer"),
        HeaderValue::from_static("resolution=merge-duplicates,return=minimal"),
    );

    let response = client
        .post(&url)
        .headers(headers)
        .json(payloads)
        .send()
        .await
        .map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("transport error: {err}"),
        })?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(StackError::SupabaseSinkHttp {
        status: status.as_u16(),
        body,
    })
}

pub async fn send_postgres_batch(
    config: &SupabaseLoggingConfig,
    db_url: &str,
    table: &str,
    payloads: &[Value],
) -> Result<()> {
    let sql = supabase_mirror::postgres_insert_sql(config, table)?;
    let client = timeout_postgres("connect", connect_postgres(db_url)).await??;
    let json_payload = Value::Array(payloads.to_vec());
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [&table, &json_payload];
    timeout_postgres("execute", client.execute(sql.as_str(), &params))
        .await?
        .map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("postgres insert failed: {err:?}"),
        })?;
    Ok(())
}

pub async fn check_postgres_table(
    config: &SupabaseLoggingConfig,
    db_url: &str,
    table: &str,
) -> Result<bool> {
    let sql = supabase_mirror::check_table_sql(config, table)?;
    let client = timeout_postgres("connect", connect_postgres(db_url)).await??;
    timeout_postgres("query", client.query_one(sql.as_str(), &[]))
        .await?
        .and_then(|row| row.try_get::<_, bool>(0))
        .map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("postgres table check failed: {err:?}"),
        })
}

async fn connect_postgres(db_url: &str) -> Result<tokio_postgres::Client> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let supabase_root = base64::engine::general_purpose::STANDARD
        .decode(SUPABASE_ROOT_2021_CA_DER_BASE64)
        .map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("supabase CA decode failed: {err}"),
        })?;
    roots
        .add(rustls::pki_types::CertificateDer::from(supabase_root))
        .map_err(|err| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("supabase CA load failed: {err}"),
        })?;
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_config);
    let (client, connection) =
        tokio_postgres::connect(db_url, tls)
            .await
            .map_err(|err| StackError::SupabaseSinkHttp {
                status: 0,
                body: format!("postgres connect failed: {err:?}"),
            })?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            tracing::warn!(error = %err, "supabase postgres connection closed");
        }
    });
    Ok(client)
}

async fn timeout_postgres<T, F>(operation: &'static str, future: F) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(POSTGRES_TIMEOUT, future)
        .await
        .map_err(|_| StackError::SupabaseSinkHttp {
            status: 0,
            body: format!("postgres {operation} timed out after {POSTGRES_TIMEOUT:?}"),
        })
}

fn record_failure_observation(
    failure_count: usize,
    error: String,
    failure_window_count: &mut i64,
    failure_window_last_error: &mut Option<String>,
) {
    if failure_count == 0 {
        return;
    }
    *failure_window_count += failure_count as i64;
    *failure_window_last_error = Some(error);
}

/// Classify an HTTP failure as permanent vs transient: 5xx, 429, and
/// transport errors retry; other 4xx are permanent.
fn classify_error(err: &StackError) -> (bool, String) {
    match err {
        StackError::SupabaseSinkHttp { status, body } => {
            let permanent = (*status >= 400 && *status < 500 && *status != 429)
                || (*status == 0 && body.contains("kind: Db"));
            (permanent, format!("HTTP {status}: {body}"))
        }
        other => (false, other.to_string()),
    }
}

/// `min(30s * 2^(attempts - 1), 1h)` ± 20% jitter.
fn backoff_delay(attempts: i64) -> Duration {
    let base_secs: u64 = 30u64.saturating_mul(1u64 << attempts.saturating_sub(1).min(7) as u32);
    let capped = base_secs.min(3600);
    let mut rng = rand::rng();
    let jitter_pct: f64 = rng.random_range(-0.2..0.2);
    let jittered = (capped as f64 * (1.0 + jitter_pct)).max(1.0) as u64;
    Duration::from_secs(jittered)
}

fn next_backoff_interval(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > POLL_INTERVAL_MAX {
        POLL_INTERVAL_MAX
    } else if doubled < POLL_INTERVAL_MIN {
        POLL_INTERVAL_MIN
    } else {
        doubled
    }
}

fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn offset_iso(now_rfc3339: &str, delta: Duration) -> String {
    chrono::DateTime::parse_from_rfc3339(now_rfc3339)
        .map(|t| t.with_timezone(&Utc) + chrono::Duration::from_std(delta).unwrap_or_default())
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Nanos, true))
        .unwrap_or_else(|_| now_rfc3339.to_owned())
}

async fn roll_failure_window(
    state: &Arc<TokioMutex<StateStore>>,
    window_started_at: &mut String,
    failure_count: &mut i64,
    last_error: &mut Option<String>,
    window_clock: &mut std::time::Instant,
) {
    if *failure_count > 0 {
        let now = current_timestamp();
        let snapshot_started = window_started_at.clone();
        let snapshot_count = *failure_count;
        let snapshot_error = last_error.clone().unwrap_or_default();
        let snapshot_now = now.clone();
        if let Err(err) = state.lock().await.record_sink_failure_window(
            &snapshot_started,
            snapshot_count,
            &snapshot_error,
            &snapshot_now,
        ) {
            tracing::warn!(error = %err, "failed to record sink failure window");
        }
    }
    *window_started_at = current_timestamp();
    *failure_count = 0;
    *last_error = None;
    *window_clock = std::time::Instant::now();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateStore;
    use tempfile::tempdir;

    #[test]
    fn backoff_doubles_then_caps_at_one_hour() {
        let d1 = backoff_delay(1).as_secs();
        assert!(
            (24..=36).contains(&d1),
            "first attempt around 30s, got {d1}"
        );
        let d10 = backoff_delay(10).as_secs();
        assert!(d10 <= 3600 + 720, "capped near 1h, got {d10}");
    }

    #[test]
    fn next_backoff_interval_doubles_to_ceiling() {
        let a = next_backoff_interval(Duration::from_secs(1));
        assert_eq!(a, Duration::from_secs(2));
        let b = next_backoff_interval(Duration::from_secs(20));
        assert_eq!(b, POLL_INTERVAL_MAX);
    }

    #[test]
    fn offset_iso_advances_by_delta() {
        let t0 = "2026-05-15T00:00:00Z";
        let later = offset_iso(t0, Duration::from_secs(60));
        assert!(later.starts_with("2026-05-15T00:01:00"), "got {later}");
    }

    #[test]
    fn classify_error_marks_4xx_as_permanent_except_429() {
        let permanent = StackError::SupabaseSinkHttp {
            status: 401,
            body: "no".into(),
        };
        let (p, _) = classify_error(&permanent);
        assert!(p);
        let transient = StackError::SupabaseSinkHttp {
            status: 429,
            body: "throttled".into(),
        };
        let (p, _) = classify_error(&transient);
        assert!(!p);
        let server = StackError::SupabaseSinkHttp {
            status: 503,
            body: "down".into(),
        };
        let (p, _) = classify_error(&server);
        assert!(!p);
        let db_error = StackError::SupabaseSinkHttp {
            status: 0,
            body: "postgres insert failed: Error { kind: Db }".into(),
        };
        let (p, _) = classify_error(&db_error);
        assert!(p);
    }

    #[test]
    fn record_failure_observation_updates_window_state() {
        let mut failure_window_count = 0;
        let mut failure_window_last_error = None;

        record_failure_observation(
            2,
            "redaction failed".to_owned(),
            &mut failure_window_count,
            &mut failure_window_last_error,
        );

        assert_eq!(failure_window_count, 2);
        assert_eq!(
            failure_window_last_error.as_deref(),
            Some("redaction failed")
        );

        record_failure_observation(
            0,
            "ignored".to_owned(),
            &mut failure_window_count,
            &mut failure_window_last_error,
        );

        assert_eq!(failure_window_count, 2);
        assert_eq!(
            failure_window_last_error.as_deref(),
            Some("redaction failed")
        );
    }

    #[tokio::test]
    async fn roll_failure_window_persists_observed_error() {
        let dir = tempdir().expect("tempdir");
        let store = StateStore::open(dir.path().join("state.sqlite")).expect("open store");
        store.migrate().expect("migrate");
        let state = Arc::new(TokioMutex::new(store));
        let mut window_started_at = "2026-01-01T00:00:00Z".to_owned();
        let mut failure_count = 1;
        let mut last_error = Some("redaction failed".to_owned());
        let mut window_clock = std::time::Instant::now();

        roll_failure_window(
            &state,
            &mut window_started_at,
            &mut failure_count,
            &mut last_error,
            &mut window_clock,
        )
        .await;

        let summary = state
            .lock()
            .await
            .latest_sink_failure_summary()
            .expect("query summary")
            .expect("summary present");
        assert_eq!(summary.1, 1);
        assert_eq!(summary.2.as_deref(), Some("redaction failed"));
        assert_eq!(failure_count, 0);
        assert!(last_error.is_none());
    }
}
