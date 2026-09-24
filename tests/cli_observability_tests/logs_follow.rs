//! `acps logs query --follow` and `acps logs tail` against a daemon that closes them for falling behind its event channel.
//!
//! The daemon runs on this test's single-threaded runtime, so while a burst of writes holds the thread its WebSocket task cannot drain the channel: one write past capacity deterministically puts the CLI's subscriber behind.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acp_stack::api::{self, AppState, RuntimePaths};
use acp_stack::config::load_config_from_str;
use acp_stack::events::EVENT_CHANNEL_CAPACITY;
use acp_stack::state::{StateStore, default_state_path};
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

use crate::common::cli::{ADMIN_KEY, SESSION_KEY, VALID_PLACEBO_CONFIG, write_cli_home};

const FOLLOW_KIND: &str = "test.follow_resume";
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
// The subscriber poll shares the CLI's IP and session key, so the fixture's limits would 429 a slow CLI handshake.
const DAEMON_RATE_LIMIT_PER_MINUTE: u64 = 60_000;
const DAEMON_RATE_LIMIT_BURST: u64 = 10_000;

/// A daemon whose state database is the one the CLI reads under the same HOME, so a `--follow` backfill sees every row the daemon writes.
struct LogsDaemon {
    base_url: String,
    home: TempDir,
    state: Arc<TokioMutex<StateStore>>,
    join: JoinHandle<acp_stack::error::Result<()>>,
}

impl LogsDaemon {
    async fn spawn() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local address"));
        write_cli_home(home.path(), &base_url, ADMIN_KEY);

        let state_path = default_state_path(home.path());
        let store = StateStore::open(&state_path).expect("state open");
        store.migrate().expect("migrate");
        let mut config = load_config_from_str(VALID_PLACEBO_CONFIG).expect("config parses");
        config.security.http.rate_limit_per_minute = DAEMON_RATE_LIMIT_PER_MINUTE;
        config.security.http.burst = DAEMON_RATE_LIMIT_BURST;
        let workspace = home.path().join("workspace");
        let uploads = workspace.join("uploads");
        std::fs::create_dir_all(&uploads).expect("workspace uploads should be created");
        config.workspace.root = workspace.to_string_lossy().into_owned();
        config.workspace.uploads = uploads.to_string_lossy().into_owned();
        config.agent.command = env!("CARGO_BIN_EXE_placebo-agent").to_owned();
        config.agent.args = vec!["acp".into()];
        config.agent.env = vec![];
        config.agent.expected_sha256 = None;
        let config_path = home.path().join(".config/acp-stack/acps-config.toml");
        let runtime_paths = RuntimePaths::new(config_path, state_path, home.path().to_path_buf());
        let app_state = AppState::with_effective_bind_and_runtime_paths(
            config,
            store,
            SESSION_KEY.to_owned(),
            ADMIN_KEY.to_owned(),
            "127.0.0.1:7700".to_owned(),
            runtime_paths,
        );
        let state = app_state.state.clone();
        let join = tokio::spawn(async move { api::serve(app_state, listener).await });
        Self {
            base_url,
            home,
            state,
            join,
        }
    }

    /// Append one `FOLLOW_KIND` row per label while holding the state lock, so the daemon's tasks cannot run until the whole batch is written.
    async fn append_rows(&self, labels: impl IntoIterator<Item = String>) {
        let store = self.state.lock().await;
        for label in labels {
            store
                .append_event("info", FOLLOW_KIND, &label, "{}")
                .expect("event inserted");
        }
    }

    /// Poll `/v1/ws/connections` until a connection subscribed to `logs` is listed.
    async fn await_logs_subscriber(&self, cli: &mut CliProcess) {
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
        loop {
            if let Some(status) = cli.child.try_wait().expect("child status") {
                panic!(
                    "acps exited with {status} before subscribing to logs; stderr: {}",
                    cli.stderr_text()
                );
            }
            let listing: Value = client
                .get(format!("{}/v1/ws/connections", self.base_url))
                .header("Authorization", format!("Bearer {SESSION_KEY}"))
                .send()
                .await
                .expect("ws connections")
                .json()
                .await
                .expect("ws connections json");
            let subscribed = listing["data"]["connections"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|connection| {
                    connection["topics"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|topic| topic == "logs")
                });
            if subscribed {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the CLI never subscribed to logs; stderr: {}",
                cli.stderr_text()
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl Drop for LogsDaemon {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// A running `acps` process whose stdout and stderr lines are collected as they arrive.
struct CliProcess {
    child: Child,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl CliProcess {
    fn spawn(daemon: &LogsDaemon, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_acps"))
            .env("HOME", daemon.home.path())
            .env_remove("ACP_STACK_TEST_DISPOSABLE_HOST")
            .env_remove("ACP_STACK_SESSION_KEY")
            .args(args)
            .args(["--session-key", SESSION_KEY])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("acps should spawn");
        let stdout = collect_lines(child.stdout.take().expect("stdout piped"));
        let stderr = collect_lines(child.stderr.take().expect("stderr piped"));
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn stdout_lines(&self) -> Vec<String> {
        self.stdout.lock().expect("stdout buffer").clone()
    }

    fn stderr_text(&self) -> String {
        self.stderr.lock().expect("stderr buffer").join("\n")
    }

    async fn await_stdout_lines(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
        while self.stdout_lines().len() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected {count} stdout lines, got {}; stderr: {}",
                self.stdout_lines().len(),
                self.stderr_text()
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn await_exit(&mut self) -> ExitStatus {
        let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("child status") {
                return status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "acps never exited; stderr: {}",
                self.stderr_text()
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl Drop for CliProcess {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("acps child was already gone at teardown: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("acps child could not be reaped: {error}");
        }
    }
}

fn collect_lines(pipe: impl std::io::Read + Send + 'static) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            sink.lock().expect("line buffer").push(line);
        }
    });
    lines
}

fn labels(prefix: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("{prefix}-{index}"))
        .collect()
}

/// The text rendering ends with the event message, which is the row label here.
fn printed_labels(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| line.split_whitespace().last().map(str::to_owned))
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn logs_follow_resumes_after_lagged_close_without_duplicates() {
    let daemon = LogsDaemon::spawn().await;
    let backfill = labels("backfill", 3);
    daemon.append_rows(backfill.clone()).await;

    let mut cli = CliProcess::spawn(
        &daemon,
        &["logs", "query", "--follow", "--kind", FOLLOW_KIND],
    );
    cli.await_stdout_lines(backfill.len()).await;
    daemon.await_logs_subscriber(&mut cli).await;

    // Printed live, so the watermark has to move past them for the reconnect's
    // backfill not to print them again.
    let live = labels("live", 2);
    daemon.append_rows(live.clone()).await;
    cli.await_stdout_lines(backfill.len() + live.len()).await;

    let burst = labels("burst", EVENT_CHANNEL_CAPACITY + 1);
    daemon.append_rows(burst.clone()).await;
    let expected: Vec<String> = [backfill, live, burst].concat();
    cli.await_stdout_lines(expected.len()).await;
    // Anything still in flight past the expected rows would be a duplicate.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let printed = printed_labels(&cli.stdout_lines());
    let divergence = printed
        .iter()
        .zip(&expected)
        .position(|(printed, expected)| printed != expected);
    assert!(
        printed.len() == expected.len() && divergence.is_none(),
        "printed {} rows for {} expected; first divergence at {divergence:?}: printed {:?}, expected {:?}",
        printed.len(),
        expected.len(),
        divergence.and_then(|index| printed.get(index)),
        divergence.and_then(|index| expected.get(index)),
    );
    let stderr = cli.stderr_text();
    assert!(
        stderr.contains("1013") && stderr.contains("lagged") && stderr.contains("reconnecting"),
        "stderr should report the lagged close and the reconnect: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn logs_tail_exits_non_zero_when_closed_for_lagging() {
    let daemon = LogsDaemon::spawn().await;
    let mut cli = CliProcess::spawn(&daemon, &["logs", "tail"]);
    daemon.await_logs_subscriber(&mut cli).await;

    daemon
        .append_rows(labels("burst", EVENT_CHANNEL_CAPACITY + 1))
        .await;

    let status = cli.await_exit().await;
    assert!(!status.success(), "tail must fail on a lagged close");
    let stderr = cli.stderr_text();
    assert!(
        stderr.contains("1013") && stderr.contains("lagged"),
        "stderr should name the close code and reason: {stderr}"
    );
}
