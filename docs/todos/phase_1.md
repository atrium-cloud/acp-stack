# Phase 1 Todo - Local Runtime Foundation

## References

- [project-spec](../specs/project-spec.md)
- [api](../specs/api/api.md)
- [acp-bridge](../specs/acp/acp-bridge.md)
- [architecture](../mgmt/architecture.md)
- [roadmap](../mgmt/roadmap.md)

## Runtime And Project Skeleton

- [x] Create the Rust workspace and primary `acps` binary.
- [ ] Add config, API, auth, state, workspace, command, and logs modules (agent bridge and supervisor modules complete; workspace and command gateway pending).
- [x] Add structured tracing initialization.
- [x] Add baseline error type and response-envelope mapping.
- [x] Add unit/integration test harness.

## Config

- [x] Define `acp-stack.toml` schema for API, auth, workspace, logging, and agent settings.
- [x] Implement config load from `~/.config/acp-stack/acp-stack.toml`.
- [x] Implement config validation with typed errors.
- [x] Implement `acps config validate [path]`.
- [x] Implement `acps config export [--output path]`.
- [x] Implement `acps config export --base64`.
- [x] Implement `acps config import <path>`.
- [x] Implement `acps config import --base64 <code>`.

## State

- [x] Create SQLite migration runner.
- [x] Add tables for sessions, events, commands, agent lifecycle records, auth failures, and installer runs.
- [x] Add repository layer for appending and querying runtime records.
- [x] Ensure state is stored under `~/.local/share/acp-stack/state.sqlite`.

## Auth

- [x] Generate session and admin API keys during `acps init`.
- [x] Store API key material through secret references, not plaintext config.
- [x] Implement constant-time API key comparison.
- [x] Enforce session-key vs admin-key route authorization.
- [x] Log failed authentication attempts without storing attempted key values.
- [x] Implement `acps auth regenerate-session-key`.
- [x] Ensure the admin key is generated only once during init and is not regenerable.

## Secrets

- [x] Generate the age x25519 identity at `~/.config/acp-stack/age.key` (`0600`).
- [x] Initialize the age-encrypted store at `~/.local/share/acp-stack/secrets.age` (`0600`).
- [x] Implement `acps secrets list`.
- [x] Implement `acps secrets set <name>`.
- [x] Implement `acps secrets delete <name>`.

## Reset

- [x] Implement `acps reset --yes` to wipe config, state, age key, and secret store.
- [x] Dry-run output and non-zero exit when `--yes` is omitted.

## HTTP And WebSocket API

- [x] Serve all public routes under `/v1`.
- [x] Implement the standard success/error response envelope.
- [x] Add request body size limits from config.
- [x] Implement status routes.
- [ ] Implement config API routes.
- [x] Implement agent lifecycle API routes.
- [ ] Implement session API route stubs wired to the ACP bridge.
- [ ] Implement workspace file API routes.
- [ ] Implement command API routes.
- [x] Implement log query API routes.
- [ ] Implement `/v1/ws` subscriptions for sessions, commands, workspace, agent, status, and logs.

## ACP Agent Bridge

- [x] Launch one configured ACP agent per runtime.
- [x] Set agent cwd to `agent.cwd` or `workspace.root`.
- [x] Inject only environment variables referenced by `[agent].env`.
- [x] Send ACP `initialize` and persist returned capabilities.
- [ ] Map session create/load/resume/close/prompt/cancel to ACP session methods where supported.
- [ ] Forward ACP `session/update` notifications to WebSocket and SQLite.
- [ ] Return typed unsupported-capability errors instead of emulating missing ACP features.

## Agent Installation

- [x] Implement declared shell installer execution.
- [x] Check `creates` before and after install.
- [x] Capture installer stdout, stderr, exit status, and timestamps.
- [x] Implement `acps agent install`.
- [x] Implement `acps agent start`.
- [x] Implement `acps agent stop`.
- [x] Implement `acps agent status`.
- [x] Verify expected binary hash when `expected_sha256` is configured.

## Workspace And Commands

- [ ] Resolve all relative workspace paths under `workspace.root`.
- [ ] Reject path traversal.
- [ ] Reject symlink escapes by default.
- [ ] Implement bounded file reads.
- [ ] Implement explicit binary downloads.
- [ ] Implement atomic writes where practical.
- [ ] Implement file upload and delete.
- [ ] Implement daemon-mediated shell command execution.
- [ ] Capture command stdout, stderr, exit status, and timing.
- [ ] Stream command output over WebSocket.

## CLI Surface

- [x] Implement `acps init`.
- [x] Implement `acps serve`.
- [x] Implement `acps status`.
- [x] Implement `acps reset [--yes]`.
- [ ] Implement `acps sessions list`.
- [ ] Implement `acps sessions new`.
- [ ] Implement `acps sessions prompt <session-id>`.
- [ ] Implement `acps sessions cancel <session-id>`.
- [ ] Implement `acps sessions close <session-id>`.
- [ ] Implement `acps logs tail`.
- [x] Implement `acps logs query`.

## Acceptance

- [x] A user can initialize config and state with `acps init`.
- [x] A user can start the daemon with `acps serve`.
- [x] A direct-key ACP agent can be installed or configured.
- [ ] A session can be created through CLI or HTTP.
- [ ] A prompt can be sent and streamed over WebSocket.
- [ ] Workspace files can be browsed, read, written, uploaded, downloaded, and deleted.
- [ ] A mediated shell command can be run and logged.
- [x] Durable logs can be queried from SQLite.
