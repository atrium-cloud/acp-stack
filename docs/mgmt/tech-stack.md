# Tech Stack

This document records the implementation technologies chosen for the standalone Linux runtime.

## Runtime Stack

- `Rust` - single deployable binary and strong process/system programming fit.
- `clap` - operator-facing CLI parsing.
- `thiserror` - typed application errors.
- `tokio` - async runtime.
- `axum` (`ws`) - HTTP server, route/middleware composition, and WebSocket upgrades for `/v1/ws`.
- `tower`, `tower-http` (`cors`, `limit`, `trace`) - middleware composition for the axum layer: request body size enforcement, structured tracing, and the CORS allowlist applied when `[security.http].allowed_origins` is non-empty.
- `dashmap` - concurrent maps backing the in-process auth-failure IP blocker (`http_hardening::AuthFailureBlocker`), the rate limiter (`http_hardening::RateLimiter`), and the process-local WebSocket connection registry. The limiter ships three token-bucket maps: per-IP (always on), per-key (post-auth, keyed by a sha256-truncated fingerprint of the bearer), and unauthenticated (stricter, pre-bearer-match). All state is in-process and clears on daemon restart.
- `http` - shared HTTP types (`StatusCode`, headers) used by the response envelope mapping.
- `reqwest` with rustls and blocking support - CLI HTTP client for daemon-backed commands such as `acps agent start` and `acps agent stop`, HTTPS `public_url` targets, the synchronous GitHub Releases install path (release metadata + asset download) used by `agent_installer`, and the async Supabase logging sink that POSTs batched rows to PostgREST with `Prefer: resolution=merge-duplicates`.
- `zeroize` - scrubbing cached API key material on drop.
- `agent-client-protocol` - the published Rust SDK for the Agent Client Protocol. We act as the ACP client and rely on it for JSON-RPC framing, request/response correlation, and the protocol schema. The `unstable_session_close` and `unstable_session_resume` SDK features are enabled so the bridge can wire every spec-required session method (`session/new`, `session/load`, `session/resume`, `session/close`, `session/prompt`, `session/cancel`). `unstable_session_model` is enabled as a compatibility fallback for agents that still expose model lists through legacy ACP session model fields instead of `configOptions`.
- `rmcp` - the official Rust Model Context Protocol SDK. Powers the `acpctl mcp serve` subcommand: the `transport-io` feature serves stdio JSON-RPC (the default agent-spawned mode), and `transport-streamable-http-server` plus a small hyper-over-Unix-socket bridge serves streamable HTTP on a UDS for clients that dial `unix:` URLs. The accompanying `client` + `transport-streamable-http-client-unix-socket` + `transport-child-process` features are pulled in for the integration tests that drive both transports.
- `hyper`, `hyper-util` - drive the streamable-HTTP-over-UDS transport for `acpctl mcp serve`: hyper provides the HTTP/1 server, hyper-util the `TokioToHyper` IO adapter and the `TowerToHyperService` glue that lets rmcp's `tower::Service` implementation run on hyper's server trait.
- `schemars` - lifted in by rmcp's `server` feature; used to describe the input/output JSON schemas of the MCP tools.
- `sha2` - SHA-256 hashing of installed agent binaries for the optional `expected_sha256` integrity check; also used by `github_release` to verify a downloaded release asset against a sibling `checksums.txt` when the registry entry declares one.
- `flate2` - gzip decoder used by `github_release` to extract `.tar.gz` release assets in-process.
- `tar` - tar archive reader paired with `flate2` for the `.tar.gz` extraction path.
- `zip` (default-features off, `deflate` only) - zip archive reader for release assets distributed as `.zip`. The narrow feature set keeps the dependency tree free of bz2/xz codecs we do not need.
- `tokio-util` (`compat`, `rt`) - bridges tokio's `AsyncRead`/`AsyncWrite` traits to the `futures` traits the ACP SDK expects when constructing `ByteStreams` over child stdio. The `rt` feature exposes `CancellationToken` for cancelling in-flight prompts when the supervisor receives `session/cancel` or shuts the agent down.
- `futures` - shared async primitives used in concert with the ACP SDK.
- `libc` - process-group signaling (`kill(-pid, SIGKILL)`) for terminating runaway installers along with their grandchildren on Unix.
- `serde`, `serde_json`, `serde_yaml`, `toml` - API payloads, generated agent config files, durable event payloads, and migration manifest parsing.
- `tokio-tungstenite` - WebSocket client. Powers `acps logs tail` for live `/v1/ws` subscription and is reused by integration tests to verify upgrade/auth/subscription behavior.
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

- Docker packaging is available for straightforward self-hosting; systemd packaging remains planned.
- Reverse proxy guides cover public TLS and edge routing while keeping runtime HTTP hardening inside `acp-stack`.
- Cloudflare Tunnel is the preferred public-edge profile: `cloudflared` runs outside the Rust binary, maps a public hostname to the loopback `acps` listener, and supplies coarse request-origin headers for observability after trusted-proxy validation. The runtime does not bundle `cloudflared` or a GeoIP database; generated mode emits local config/systemd/Docker snippets only.
- Dependency checks and supported `deps apply` flows should stay narrow and explicit in the initial release.
