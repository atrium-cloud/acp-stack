# Phase 1 Todo - Local Runtime Foundation

## References

- [project-spec](../specs/project-spec.md)
- [api](../specs/api/api.md)
- [acp-bridge](../specs/acp/acp-bridge.md)
- [architecture](../mgmt/architecture.md)
- [roadmap](../mgmt/roadmap.md)

## Runtime And Project Skeleton

- [x] Create the Rust workspace and primary `acps` binary.
- [x] Add config, API, auth, state, workspace, command, and logs modules.
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
- [x] Implement config API routes.
- [x] Implement agent lifecycle API routes.
- [x] Implement session API route stubs wired to the ACP bridge.
- [x] Implement workspace file API routes.
- [x] Implement command API routes.
- [x] Implement log query API routes.
- [x] Implement `/v1/ws` subscriptions for sessions, commands, workspace, agent, status, and logs (permissions topic deferred to the dedicated module).

## ACP Agent Bridge

- [x] Launch one configured ACP agent per runtime.
- [x] Set agent cwd to `agent.cwd` or `workspace.root`.
- [x] Inject only environment variables referenced by `[agent].env`.
- [x] Send ACP `initialize` and persist returned capabilities.
- [x] Map session create/load/resume/close/prompt/cancel to ACP session methods where supported.
- [x] Forward ACP `session/update` notifications to WebSocket and SQLite.
- [x] Return typed unsupported-capability errors instead of emulating missing ACP features.

## Agent Installation

- [x] Implement declared shell installer execution.
- [x] Check `creates` before and after install.
- [x] Capture installer stdout, stderr, exit status, and timestamps.
- [x] Implement `acps agent install`.
- [x] Implement `acps agent start`.
- [x] Implement `acps agent stop`.
- [x] Implement `acps agent status`.
- [x] Verify expected binary hash when `expected_sha256` is configured.

## Agent Registry & Two-Layer Install

The original installer landed without a curated embedded registry or a model that can represent adapter-backed agents later. The work below is a Phase 1 gap-fill, not a rework: the items above are still correct for native ACP agents; the items below replace runtime upstream-registry fetches with a narrow embedded catalog, starting with OpenCode, Cursor CLI, Amp, and Pi as verified headless targets.

- [x] Hand-curate `data/agents.toml` with `kind = "native" | "adapter"` per entry, starting with OpenCode, Cursor CLI, Amp, and Pi.
- [x] Add `src/runtime/install/agent_registry.rs` with `include_str!`-loaded embedded catalog and optional override at `~/.config/acp-stack/agents.toml`.
- [x] Add `src/runtime/install/github_release.rs` with API client, asset glob matching (`{arch}` substitution), `tar.gz` / `zip` / raw extraction, optional `checksums.txt` verification, and `GITHUB_TOKEN` passthrough.
- [x] Remove upstream registry fetch from `src/runtime/install/agent_installer.rs`; resolve installs from the embedded catalog.
- [x] Refactor `agent_installer` to dispatch on `kind` and orchestrate two install steps (harness then adapter) for adapter-backed entries.
- [x] Add `step` column to `installer_runs` (migration 009) and thread it through `InstallerRunInput` / persistence.
- [x] Update `[agent]` config: drop operator-written `[agent.adapter]` (now runtime-populated and rejected via `skip_deserializing`), make `[agent.install]` optional (escape hatch only), add `harness_version` for GitHub Release tag pinning.
- [x] Wire adapter metadata population from the resolved registry entry into the in-memory `AgentConfig` at app startup so the existing `/v1/agent/capabilities` and `/v1/status/agent` responses keep working.
- [x] Add error variants in `src/error.rs` for GitHub Release paths and registry-load failures; drop now-dead `AgentRegistry{Fetch,Parse,UnsupportedDistribution}`.
- [x] Add dev-only `sync-registry-check` binary that verifies embedded registry ids still exist upstream.
- [x] Update `docs/specs/runtime.md`, `docs/specs/config.md`, and `docs/specs/acp/acp-bridge.md` to describe the new install model.
- [x] Update `docs/mgmt/architecture.md` and `docs/mgmt/tech-stack.md` for new modules and crates.

## Agent Headless Support Contracts

- [x] Make OpenCode with OpenCode Go the first verified headless support target.
- [x] Define the support contract format for documented agent harnesses.
- [x] For every `headless_compatible = true` registry entry, add `docs/agents/{id}.md`.
- [x] Each agent doc cites the official docs/repos used as sources.
- [x] Each agent doc defines install method, ACP launch command, auth flow, required env vars, optional env vars, and provider/model setup where applicable.
- [x] Classify each variable as secret, non-secret config, install-only, or runtime env.
- [x] Document unsupported auth paths, especially OAuth-only or browser-login flows.
- [x] Add a minimal non-interactive smoke verification for each supported agent.
- [x] Add registry metadata linking each supported entry to its headless setup doc.
- [x] Keep every other registry entry unverified until its own headless setup doc and smoke verification exists.
- [x] Treat missing or non-credible headless setup docs as a blocker for `headless_compatible = true`.
- [x] Add a reusable API-key env var to provider-id mapping for init and provider/model resolution.
- [x] During `acps init`, provision baseline OpenCode and Pi config files so env-injected API keys are actually consumed headlessly.

Phase 1 scope is limited to credible headless agent support. Full installer orchestration, workspace data ingestion, and an operator-facing real-prompt testflight command are Phase 4 work.

## Workspace And Commands

- [x] Resolve all relative workspace paths under `workspace.root`.
- [x] Reject path traversal.
- [x] Reject symlink escapes by default.
- [x] Implement bounded file reads.
- [x] Implement explicit binary downloads.
- [x] Implement atomic writes where practical.
- [x] Implement file upload and delete.
- [x] Implement daemon-mediated shell command execution.
- [x] Capture command stdout, stderr, exit status, and timing.
- [x] Stream command output over WebSocket.

## CLI Surface

- [x] Implement `acps init`.
- [x] Implement `acps serve`.
- [x] Implement `acps status`.
- [x] Implement `acps reset [--yes]`.
- [x] Implement `acps sessions list`.
- [x] Implement `acps sessions new`.
- [x] Implement `acps sessions prompt <session-id>`.
- [x] Implement `acps sessions cancel <session-id>`.
- [x] Implement `acps sessions close <session-id>`.
- [x] Implement `acps logs tail`.
- [x] Implement `acps logs query`.

## Acceptance

- [x] A user can initialize config and state with `acps init`.
- [x] A user can start the daemon with `acps serve`.
- [x] A direct-key ACP agent can be installed or configured.
- [x] A session can be created through CLI or HTTP.
- [x] A prompt can be sent and streamed over WebSocket.
- [x] Workspace files can be browsed, read, written, uploaded, downloaded, and deleted.
- [x] A mediated shell command can be run and logged.
- [x] Durable logs can be queried from SQLite.
