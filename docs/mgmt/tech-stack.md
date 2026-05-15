# Tech Stack

This document records the implementation technologies chosen for the standalone Linux runtime.

## Runtime Stack

- `Rust` - single deployable binary and strong process/system programming fit.
- `clap` - operator-facing CLI parsing.
- `thiserror` - typed application errors.
- `tokio` - async runtime.
- `axum` (`ws`) - HTTP server, route/middleware composition, and WebSocket upgrades for `/v1/ws`.
- `tower`, `tower-http` - middleware composition (body limits, tracing) for the axum layer.
- `http` - shared HTTP types (`StatusCode`, headers) used by the response envelope mapping.
- `reqwest` with rustls - CLI HTTP client for daemon-backed commands such as `acps agent start` and `acps agent stop`, including HTTPS `public_url` targets.
- `zeroize` - scrubbing cached API key material on drop.
- `agent-client-protocol` - the published Rust SDK for the Agent Client Protocol. We act as the ACP client and rely on it for JSON-RPC framing, request/response correlation, and the protocol schema. The `unstable_session_close` and `unstable_session_resume` SDK features are enabled so the bridge can wire every spec-required session method (`session/new`, `session/load`, `session/resume`, `session/close`, `session/prompt`, `session/cancel`).
- `sha2` - SHA-256 hashing of installed agent binaries for the optional `expected_sha256` integrity check.
- `tokio-util` (`compat`, `rt`) - bridges tokio's `AsyncRead`/`AsyncWrite` traits to the `futures` traits the ACP SDK expects when constructing `ByteStreams` over child stdio. The `rt` feature exposes `CancellationToken` for cancelling in-flight prompts when the supervisor receives `session/cancel` or shuts the agent down.
- `futures` - shared async primitives used in concert with the ACP SDK.
- `libc` - process-group signaling (`kill(-pid, SIGKILL)`) for terminating runaway installers along with their grandchildren on Unix.
- `serde`, `serde_json`, `toml` - API payloads, config files, durable event payloads, and migration manifest parsing.
- `tokio-tungstenite` - WebSocket client used by integration tests to verify `/v1/ws` upgrade/auth/subscription behavior.
- `chrono` - RFC3339 timestamps for durable state records.
- `rusqlite` - SQLite state and migrations.
- `tokio::process` - agent, MCP, and command execution.
- `portable-pty` - optional PTY allocation for terminal-like command sessions.
- `tracing` - structured logs.
- `tracing-subscriber` - local tracing subscriber initialization.
- `base64` - portable config export encoding.
- `notify` - workspace file event streaming.
- `age` - age-compatible secret encryption (encrypts `secrets.age`).
- `rand` - cryptographically secure random bytes for API key generation.
- `subtle` - constant-time byte-slice comparison for API key validation.
- `tempfile` - durable atomic-write helpers (temp file + rename).
- `reqwest` (dev) - HTTP client driving end-to-end API integration tests.

## Storage and Data Contracts

- SQLite is the local source of truth for sessions, events, commands, permissions, lifecycle records, dependency checks, and derived metrics.
- PostgreSQL/Supabase mirrors use the same logical migration sequence as SQLite where external logging is enabled.
- TOML stores portable desired configuration, while age-compatible encryption stores secret values outside config.

## Deployment-Relevant Tooling

- Docker and systemd packaging are planned for straightforward self-hosting.
- Reverse proxy guides should cover public TLS and edge routing while keeping runtime HTTP hardening inside `acp-stack`.
- Dependency checks and supported `deps apply` flows should stay narrow and explicit in the 0.0.x line.
