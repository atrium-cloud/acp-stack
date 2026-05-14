# Tech Stack

This document records the implementation technologies chosen for the standalone Linux runtime.

## Runtime Stack

- `Rust` - single deployable binary and strong process/system programming fit.
- `clap` - operator-facing CLI parsing.
- `thiserror` - typed application errors.
- `tokio` - async runtime.
- `axum` - HTTP server and WebSocket upgrades.
- `tower`, `tower-http` - middleware composition (body limits, tracing) for the axum layer.
- `http` - shared HTTP types (`StatusCode`, headers) used by the response envelope mapping.
- `zeroize` - scrubbing cached API key material on drop.
- `agentclientprotocol/rust-sdk` - ACP protocol implementation where suitable.
- `serde`, `serde_json`, `toml` - API payloads, config files, durable event payloads, and migration manifest parsing.
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
