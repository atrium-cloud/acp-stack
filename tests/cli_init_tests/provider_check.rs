//! The hosted init provider check, driven over the bootstrap API of `acps init serve` the way a
//! hosting client drives it. The agent is the placebo behind a shim that rejects every prompt
//! unless the deposited credential is the accepted one.

use acp_stack::dev_gates::{
    DEV_PLACEBO_REGISTRY_ENV, TEST_DISPOSABLE_HOST_ENV, TEST_SKIP_AGENT_INSTALL_ENV,
    TEST_SKIP_NODE_RUNTIME_ENV,
};
use serde_json::{Map, Value, json};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const INIT_TOKEN: &str = "provider-check-test-token";
const ACCEPTED_KEY: &str = "accepted-test-key";
const REJECTED_KEY: &str = "rejected-test-key";
const CREDENTIAL_NAMESPACE: &str = "credential-state";
const DEPOSITED_SECRET: &str = "ROUTER_TEST_KEY";
const LISTENING_PREFIX: &str = "acps init serve: listening on ";
const SESSION_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One `acps init serve` process over a temp HOME, killed on drop.
struct InitServer {
    child: Child,
    base_url: String,
    http: reqwest::blocking::Client,
    shim: PathBuf,
    workspace: PathBuf,
    _tempdir: tempfile::TempDir,
}

impl InitServer {
    fn spawn() -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let shim = tempdir.path().join("credential-gated-placebo");
        write_credential_gated_placebo(&shim);
        let workspace = tempdir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        let mut child = Command::new(env!("CARGO_BIN_EXE_acps"))
            .args(["init", "serve", "--bind", "127.0.0.1:0"])
            .env("HOME", tempdir.path())
            .env("ACP_STACK_INIT_TOKEN", INIT_TOKEN)
            .env(DEV_PLACEBO_REGISTRY_ENV, &shim)
            .env(TEST_SKIP_AGENT_INSTALL_ENV, "1")
            .env(TEST_SKIP_NODE_RUNTIME_ENV, "1")
            // An inherited opt-out would lift the fixture guards in the spawned binary.
            .env_remove(TEST_DISPOSABLE_HOST_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn acps init serve");
        let stderr = child.stderr.take().expect("server stderr");
        let mut lines = std::io::BufReader::new(stderr).lines();
        let address = loop {
            let line = lines
                .next()
                .expect("server exited before listening")
                .expect("read server stderr");
            if let Some(address) = line.strip_prefix(LISTENING_PREFIX) {
                break address.to_owned();
            }
        };
        // Drained so the server never blocks on a full pipe, and shown with a failing test.
        std::thread::spawn(move || {
            lines
                .map_while(Result::ok)
                .for_each(|line| eprintln!("{line}"));
        });
        Self {
            child,
            base_url: format!("http://{address}"),
            http: reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .expect("http client"),
            shim,
            workspace,
            _tempdir: tempdir,
        }
    }

    fn send(&self, request: reqwest::blocking::RequestBuilder) -> Value {
        let response = request
            .bearer_auth(INIT_TOKEN)
            .send()
            .expect("bootstrap request");
        let status = response.status();
        let body: Value = response.json().expect("json body");
        assert!(status.is_success(), "HTTP {status}: {body}");
        body["data"].clone()
    }

    fn post(&self, path: &str, body: Value) -> Value {
        self.send(
            self.http
                .post(format!("{}{path}", self.base_url))
                .json(&body),
        )
    }

    fn start(&self, body: Value) -> String {
        let started = self.post("/v1/init/sessions", body);
        started["session_id"]
            .as_str()
            .expect("session id")
            .to_owned()
    }

    fn deposit(&self, revision: i64, key: &str) {
        self.post(
            "/v1/init/credential",
            json!({
                "secrets": [{ "name": DEPOSITED_SECRET, "value": key }],
                "namespace": CREDENTIAL_NAMESPACE,
                "apply": {
                    "schema_version": 1,
                    "revision": revision,
                    "desired": {
                        "kind": "provider-credential",
                        "selection": {
                            "provider_id": "openrouter",
                            "source_refs": { "OPENROUTER_API_KEY": DEPOSITED_SECRET },
                        },
                    },
                },
            }),
        );
    }

    fn close(&self, session_id: &str) {
        self.post(
            &format!("/v1/init/sessions/{session_id}/discovery/close"),
            Value::Null,
        );
    }

    /// Poll the session until `done` holds, answering the setup prompts this flow streams.
    fn advance(&self, session_id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + SESSION_TIMEOUT;
        let mut answered = None;
        loop {
            let status = self.send(
                self.http
                    .get(format!("{}/v1/init/sessions/{session_id}", self.base_url)),
            );
            if done(&status) {
                return status;
            }
            assert_ne!(status["status"], "errored", "session errored: {status}");
            assert!(Instant::now() < deadline, "timed out at {status}");
            if let Some(request_id) = status["pending_input"]["request_id"].as_str()
                && answered.as_deref() != Some(request_id)
            {
                assert_eq!(
                    status["pending_input"]["kind"], "mcp_add",
                    "unexpected prompt: {status}"
                );
                self.post(
                    &format!("/v1/init/sessions/{session_id}/input"),
                    json!({ "request_id": request_id, "value": false }),
                );
                answered = Some(request_id.to_owned());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn parked_after_checks(&self, session_id: &str, checks: usize) -> Value {
        self.advance(session_id, |status| {
            status["status"] == "awaiting_discovery_close"
                && provider_checks(status).len() == checks
        })
    }

    fn completed(&self, session_id: &str) -> Value {
        self.advance(session_id, |status| {
            status["status"] == "completed_awaiting_ack"
        })
    }

    /// A provider-backed agent whose credential arrives by deposit after the session starts.
    fn deferred_credential_body(&self, provider_check: bool) -> Value {
        let mut extensions = Map::new();
        extensions.insert(
            CREDENTIAL_NAMESPACE.to_owned(),
            json!({ "type": "managed-state", "capability": "provider-credential" }),
        );
        json!({
            "agent": "opencode",
            "provider": "openrouter",
            "defer_provider_credentials": true,
            "provider_check": provider_check,
            "extensions": extensions,
            "workspace_root": self.workspace,
            "sandbox": "off",
            "skip_testflight": true,
            "stack_update": "off",
            "agent_update": "off",
        })
    }
}

impl Drop for InitServer {
    fn drop(&mut self) {
        // The server exits on its own once a session ends, so either call may find it gone.
        if let Err(error) = self.child.kill() {
            eprintln!("stopping init server: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("reaping init server: {error}");
        }
    }
}

/// The placebo, rejecting every prompt with an upstream 401 unless the agent was handed the
/// accepted key.
fn write_credential_gated_placebo(path: &Path) {
    let placebo = env!("CARGO_BIN_EXE_placebo-agent");
    let body = format!(
        "#!/bin/sh\n\
         if [ \"$OPENROUTER_API_KEY\" = '{ACCEPTED_KEY}' ]; then\n  exec '{placebo}' acp\nfi\n\
         exec '{placebo}' acp --prompt-inference-error 'HTTP/1.1 401 Unauthorized'\n"
    );
    std::fs::write(path, body).expect("shim write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("shim chmod");
    }
}

/// The `provider_check` signals so far, reduced to their verdict fields.
fn provider_checks(status: &Value) -> Vec<Value> {
    status["signals"]
        .as_array()
        .expect("signal replay")
        .iter()
        .filter(|signal| signal["signal"] == "provider_check")
        .map(|signal| {
            json!({
                "outcome": signal["outcome"],
                "code": signal["code"],
                "status_code": signal["status_code"],
                "reason_category": signal["reason_category"],
            })
        })
        .collect()
}

fn passed() -> Value {
    json!({ "outcome": "passed", "code": "ok", "status_code": null, "reason_category": null })
}

#[test]
fn provider_check_passes_on_the_close_after_a_good_deposit() {
    let server = InitServer::spawn();
    let session_id = server.start(server.deferred_credential_body(true));

    // The credential is still pending, so no discovery prompt opened the phase; it parks anyway.
    server.parked_after_checks(&session_id, 0);
    server.deposit(1, ACCEPTED_KEY);
    server.close(&session_id);

    let status = server.completed(&session_id);
    assert_eq!(provider_checks(&status), [passed()]);
}

#[test]
fn a_failed_provider_check_reparks_until_a_higher_revision_passes() {
    let server = InitServer::spawn();
    let session_id = server.start(server.deferred_credential_body(true));
    server.parked_after_checks(&session_id, 0);
    server.deposit(1, REJECTED_KEY);
    server.close(&session_id);

    let status = server.parked_after_checks(&session_id, 1);
    assert_eq!(
        provider_checks(&status),
        [json!({
            "outcome": "failed",
            "code": "prompt_failed",
            "status_code": 401,
            "reason_category": "client_error",
        })]
    );
    assert_eq!(status["discovery"]["state"], "awaiting_close");

    server.deposit(2, ACCEPTED_KEY);
    server.close(&session_id);
    let status = server.completed(&session_id);
    assert_eq!(provider_checks(&status)[1], passed());
}

#[test]
fn provider_check_skips_an_agent_the_agent_test_cannot_drive() {
    let server = InitServer::spawn();
    let session_id = server.start(json!({
        "custom_agent_id": "house-agent",
        "custom_agent_command": server.shim,
        "custom_agent_install": "true",
        "provider_check": true,
        "workspace_root": server.workspace,
        "sandbox": "off",
        "skip_testflight": true,
        "stack_update": "off",
    }));
    server.parked_after_checks(&session_id, 0);
    server.close(&session_id);

    let status = server.completed(&session_id);
    assert_eq!(
        provider_checks(&status),
        [json!({
            "outcome": "skipped",
            "code": "agent_unsupported",
            "status_code": null,
            "reason_category": null,
        })]
    );
}

#[test]
fn without_provider_check_a_run_that_asked_nothing_never_parks() {
    let server = InitServer::spawn();
    let session_id = server.start(server.deferred_credential_body(false));

    // Completing with no close sent is the proof: a parked run would wait for one.
    let status = server.completed(&session_id);
    assert!(provider_checks(&status).is_empty(), "{status}");
}
