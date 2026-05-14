# Tech Stack

This document records the implementation technologies chosen for the standalone Linux runtime.

## Runtime Stack

- `Rust` - single deployable binary and strong process/system programming fit.
- `clap` - operator-facing CLI parsing.
- `thiserror` - typed application errors.
- `tokio` - async runtime.
- `axum` - HTTP server and WebSocket upgrades.
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
- `age` or `rage` - age-compatible secret encryption.

## Storage and Data Contracts

- SQLite is the local source of truth for sessions, events, commands, permissions, lifecycle records, dependency checks, and derived metrics.
- PostgreSQL/Supabase mirrors use the same logical migration sequence as SQLite where external logging is enabled.
- TOML stores portable desired configuration, while age-compatible encryption stores secret values outside config.

## Deployment-Relevant Tooling

- Docker and systemd packaging are planned for straightforward self-hosting.
- Reverse proxy guides should cover public TLS and edge routing while keeping runtime HTTP hardening inside `acp-stack`.
- Dependency checks and supported `deps apply` flows should stay narrow and explicit in the 0.0.x line.
