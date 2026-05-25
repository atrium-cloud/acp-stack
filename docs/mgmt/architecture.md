# Architecture

This document captures the management-level architecture for `acp-stack`. For concrete API routes and runtime behavior, see [api](../specs/api/api.md) and [project-spec](../specs/project-spec.md).

## Overview

`acp-stack` is a single Rust binary with a modular internal architecture:

```text
+---------------------------------------------+
|                 Unified API                 |
|             HTTP + WebSocket v1             |
+---------------------------------------------+
| Auth | Config | Status | Logs | Permissions |
+-----------------------+---------------------+
| Workspace + Commands  | Agent Sessions      |
| Files | Uploads | Sh  | ACP Bridge          |
+-----------------------+---------------------+
| Runtime Supervisor | MCP Launcher | Secrets |
+---------------------------------------------+
| SQLite State | Config TOML | Age Secret Key |
+---------------------------------------------+
                    |
                    v
             Linux Environment
        Docker / VM / Bare Metal / Hosted
```

### Core Modules

- `Config` - reads, validates, imports, exports, and applies `acp-stack.toml`.
- `API` - axum HTTP routes and WebSocket event streaming.
- `Auth` - two-tier API key validation and request authorization.
- `State` - SQLite migrations and repositories for sessions, events, commands, permissions, and lifecycle records.
- `ACP Bridge` - launches the configured agent and speaks ACP JSON-RPC over stdio.
- `Runtime Supervisor` - owns process lifecycle for the active agent and MCP server processes.
- `Workspace` - bounded file operations, uploads/downloads, and workspace path policy.
- `Command Gateway` - launches shell commands through `acp-stack`, evaluates policy, records output, and creates permission requests when needed.
- `Secrets` - age key management, encrypted secret store, secret references, and scoped env injection.
- `Dependencies` - validates declared tools/runtimes/packages and reports missing items.
- `Permissions` - durable request/decision lifecycle for ACP permission requests and stack-mediated commands.
- `Events` - normalizes WebSocket messages and durable event records.
- `Edge` - renders generated Cloudflare Tunnel artifacts and carries bounded request-origin metadata through HTTP hardening.

The current Rust crate exposes a library behind the `acps` daemon binary and the `acpctl` local-agent binary. Runtime implementation files are grouped by domain: `runtime::agent`, `runtime::install`, `runtime::dependencies`, `runtime::mediation`, `runtime::workspace_sources`, and `runtime::logging`. Cross-cutting runtime orchestration stays at `runtime::init_runner` and `runtime::health`. These grouped paths are the canonical runtime module paths; old flat aliases such as `runtime::acp_bridge` and root aliases such as `crate::supervisor` are intentionally not re-exported. The HTTP API surface splits into per-route-group leaves under `api/routes/` plus auth/ws middleware leaves under `api/`; durable SQLite state splits into per-table leaves under `state/`; the CLI splits per subcommand group under `cli/`; and the local `acpctl` listener keeps a public `local_listener` facade with router/socket internals under `local_listener/`. The `acpctl` binary entrypoint is `src/bin/acpctl/main.rs`, with command dispatch, UDS HTTP client, helpers, and formatters in sibling modules. The `acpctl mcp serve` subcommand owns its own submodule tree under `src/bin/acpctl/mcp/` (`dispatcher`, `tools`, `server`, `transport_uds`) — an rmcp-based MCP server that re-uses the UDS HTTP client to proxy every tool call into the daemon's existing allowlisted local routes.

### Source tree

```text
src/
  lib.rs                          public crate module declarations + shared Result/Error exports
  main.rs                         acps daemon entry
  api.rs                          public api surface; re-exports from leaves
  api/
    core.rs                       AppState, build_router, serve, shutdown_signal
    auth.rs                       auth + tier + envelope + request-tracking middleware
    ws.rs                         /v1/ws upgrade and subscription loop
    routes.rs                     route submodule declarations
    routes/
      agent.rs                    /v1/agent/* handlers
      commands.rs                 /v1/commands* handlers
      config.rs                   /v1/config/* + /v1/secrets/* handlers
      deps.rs                     /v1/deps* handlers
      logs.rs                     /v1/logs/* handlers + shared paging helpers
      metrics.rs                  /v1/metrics/summary + JSON view types
      permissions.rs              /v1/permissions/* handlers
      security.rs                 /v1/security/check
      sessions.rs                 /v1/sessions/* handlers + session helpers
      status.rs                   /v1/status* handlers
      workspace.rs                /v1/workspace + /v1/files* handlers
  state.rs                        public state surface; re-exports from leaves
  state/
    core.rs                       StateStore + connection lifecycle + accessors
    schema.rs                     migration runner + embedded DDL
    ids.rs                        record id generators + current_timestamp
    rows.rs                       shared json validation + events-query predicates
    records.rs                    shared filter DTOs (LogFilter / Session / Command)
    events.rs                     events table + EVENT_SOURCE_* labels
    sessions.rs                   sessions + prompts persistence
    commands.rs                   commands table persistence + output chunks
    permissions.rs                permission_requests + permission_decisions
    auth.rs                       auth_failures persistence
    agent.rs                      agent_lifecycle + capabilities + installer_runs
    init.rs                       init_runs + init_steps (init orchestrator)
    metrics.rs                    derived metrics aggregations
    sink_outbox.rs                Supabase delivery outbox + hydration + bookkeeping
  cli.rs                          re-exports Cli, run
  cli/
    core.rs                       top-level Cli enum + run() dispatch + shared helpers
    init.rs, serve.rs, status.rs, reset.rs, agent.rs, sessions.rs, logs.rs,
    auth.rs, secrets.rs, config.rs, deps.rs, metrics.rs   per-subcommand handlers
  runtime.rs                      grouped runtime module declarations
  runtime/
    init_runner.rs                init step orchestrator (record_step + verifier resume)
    health.rs                     unified daemon health report (SQLite, workspace, agent, sink, deps) for `/v1/health/*` and `acps status`
    agent.rs                      agent runtime facade
    agent/
      acp_bridge.rs               ACP client wrapping agent-client-protocol SDK
      agent_headless_config.rs     generated config for supported headless agents
      mcp.rs                      MCP server resolution from config + secrets
      model_discovery.rs          session-config and model discovery
      provider_keys.rs            API-key env var to provider-id mapping
      supervisor.rs               agent process supervisor + prompt registry
    install.rs                    install/runtime catalog facade
    install/
      agent_installer.rs          shell + registry-resolved installer
      agent_registry.rs           embedded data/agents.toml loader + lookup
      github_release.rs           GitHub Release-driven binary installer
      npm_registry.rs             npm registry lookup helper
    dependencies.rs               dependency facade
    dependencies/
      deps.rs                     declarative dependency checker
      deps_apply.rs               `acps deps apply` runner (narrow declared shell snippets)
    mediation.rs                  mediated operation facade
    mediation/
      commands.rs                 command gateway (policy + spawn + stream)
      permissions.rs              permission lifecycle service + waiter map
    workspace_sources.rs          workspace source materialization facade
    workspace_sources/
      workspace_init.rs           Phase 4 `[[workspace.code_sources]]` + `[[workspace.data_sources]]` materializer
      safe_download.rs            streaming HTTPS downloader with redirect, size, and scheme caps
      safe_extract.rs             tar/tar.gz/zip extractor with traversal, symlink, size guards
      s3_client.rs                minimal SigV4-aware S3 client (ListObjectsV2 + GetObject) for workspace ingest
    logging.rs                    external logging facade
    logging/
      sink_redaction.rs           per-table allowlists for Supabase upload
      supabase_sink.rs            outbox-driven Supabase logging sink worker
  local_listener.rs               UDS listener public surface
  local_listener/
    router.rs                     allowlist axum router on top of api handlers
    socket.rs                     UDS bind / permissions / lifecycle
  bin/acpctl/                     local-agent CLI binary
    main.rs                       entry point
    app.rs                        command dispatch
    cli_defs.rs                   clap definitions
    client.rs                     UDS HTTP client
    formatters.rs                 stdout/JSON formatters
    helpers.rs                    socket-path + url-encoding helpers
    mcp.rs                        `acpctl mcp serve` subcommand entry
    mcp/
      dispatcher.rs               tool name + args → UDS request mapping
      tools.rs                    rmcp Tool definitions for the 10-tool surface
      server.rs                   rmcp ServerHandler + stdio transport entry
      transport_uds.rs            streamable-HTTP MCP transport bound on UDS
  auth.rs                         KeyKind + constant-time compare + failure recording
  config.rs                       TOML schema, load/validate/canonicalize
  envelope.rs                     ApiSuccess / ApiError + IntoResponse for StackError
  error.rs                        StackError + Result type
  events.rs                       in-process broadcast hub + live event envelope
  fs_util.rs                      owner-only fs helpers
  http_hardening.rs               IP block + rate limit + CORS + origin allowlist + origin metadata
  edge.rs                         Cloudflare Tunnel artifact generation
  ownership.rs                    path posture + getpwnam_r + workspace writability
  secrets.rs                      age-encrypted secret store
  security.rs                     runtime security self-check
  time_util.rs                    duration-suffix parsing
  tracing_init.rs                 structured tracing setup
  workspace.rs                    workspace path resolver + file operations
```

`local_listener` owns the Unix-domain-socket transport that serves `acpctl`: it builds an explicit-allowlist Axum router on top of `api` handlers, stamps every request with `KeyKind::Local` via a small middleware, and binds the socket at owner-only (0600 file in a 0700 directory) permissions inside `~/.local/share/acp-stack/`. The `security` module owns the runtime security self-check (`security::check`) called by both the admin-tier HTTP route and the local UDS route. `api::RuntimePaths` carries the exact config and state DB paths into handlers so the check inspects the running daemon's managed files. `ownership` is the seam between the security check and the filesystem: `inspect()` returns a `PathPosture { uid, mode, is_symlink }` for each runtime-managed path, `resolve_runtime_user_uid()` resolves `workspace.runtime_user` to a uid via `getpwnam_r`, `process_euid()` wraps `libc::geteuid`, and `workspace_writable()` probes write access with a `NamedTempFile`. The same module is intended to back ownership validation in future deployment automation (Docker / systemd installer). `time_util` parses operator-facing duration suffixes (`30m` / `1h` / `2d` / `1w`) used by `acps logs query` and the metrics summary endpoints; the rest of the runtime continues to use chrono's RFC3339 helpers directly. `runtime::mediation::permissions` owns the durable permission lifecycle (request/decide/cancel/expire) and the in-process waiter map that resolves blocked operations once a decision lands; `runtime::dependencies::deps` reports declared dependency status without installing; `runtime::agent::mcp` resolves `[mcp.servers]` entries against the secret store and hands the SDK `McpServer` list to the bridge at session create/load/resume time; `http_hardening` owns `client_ip` selection under trusted proxies, Cloudflare-aware bounded origin metadata, the CORS layer construction, the WebSocket Origin allowlist check, and the in-process auth-failure IP blocker that short-circuits brute-force attempts before bearer comparison. `edge` renders generated Cloudflare Tunnel config, systemd, Docker Compose, and operator checklist artifacts. `runtime::mediation::commands` owns the Command Gateway: it evaluates `[permissions]` glob policy, spawns shell children through `[workspace].default_shell -c`, streams stdout/stderr through `EventHub` to `commands.{id}` subscribers, persists bounded output chunks to the `events` table, and handles cancel/timeout via process-group signals. `api` owns the axum HTTP/WebSocket layer (router, auth middleware, response envelope wiring, `/v1/ws` subscription handling, and the process-local WebSocket registry/management routes), `events` owns the in-process broadcast hub and stable live event envelope, `runtime::agent::supervisor` records the daemon's lifecycle transitions and owns the spawned ACP agent's lifecycle (`AgentSupervisor`), and `runtime::agent::acp_bridge` wraps the `agent-client-protocol` SDK to spawn and initialize the configured agent. `runtime::agent::provider_keys` maps API-key env vars to valid provider ids; `runtime::agent::agent_headless_config` uses that mapping to write the baseline agent-owned config files needed for supported headless agents such as OpenCode and Pi to consume env-injected API keys. `runtime::install::agent_registry` parses the binary-embedded `data/agents.toml` (with optional operator override at `~/.config/acp-stack/agents.toml`) and looks up agents by id; `runtime::install::github_release` handles the GitHub Releases install path (API query, asset glob with `{arch}` substitution, optional `checksums.txt` verification, `tar.gz`/`zip`/raw extraction into `~/.local/bin/`); `runtime::install::agent_installer` installs native harnesses directly and runs adapter-backed harness/adapter install steps concurrently, persisting per-step rows in `installer_runs` tagged `step = "install" | "harness" | "adapter"`. `runtime::init_runner` is the higher-level orchestrator that wraps every executed or resumed `acps init` phase (secrets, agent install, provider, workspace, headless config, edge artifacts, init-complete event, testflight) in a `record_step` call that writes an `init_steps` row and consults a per-step verifier on resume — succeeded steps whose postcondition still holds are replayed as `skipped`, everything else re-executes. `acps init --resume [--run-id <id>]` continues a prior non-terminal run; `acps init --fresh` always begins a new row. `workspace` provides the workspace-path resolver and the list/read/write/upload/delete primitives behind `/v1/workspace` and `/v1/files*`. `runtime::workspace_sources::safe_download` is a Phase 4 streaming HTTPS downloader with redirect, scheme, and size caps, plus optional sha256 verification — used by `runtime::workspace_sources::workspace_init` to ingest untrusted archives without buffering the full body. `runtime::workspace_sources::safe_extract` is the paired tar/tar.gz/zip extractor that detects format by magic bytes and rejects parent-dir traversal, absolute paths, symlinks, hardlinks, special devices, and entries that exceed the per-entry or cumulative size limits. `runtime::workspace_sources::workspace_init` orchestrates the Phase 4 ingestion lanes: `[[workspace.code_sources]]` entries become `git clone` invocations beneath `<workspace.root>/usr/code/<repo-name>/`, and `[[workspace.data_sources]]` entries (local, https, s3) become copies / downloads / extractions beneath `<workspace.root>/usr/data/<name>/`. Each completed source drops a `.acp-stack-source.json` sentinel so reruns skip cleanly; a non-empty destination without a matching sentinel hard-fails. `runtime::workspace_sources::s3_client` is a minimal SigV4-aware client (HMAC-SHA256 signing, `ListObjectsV2` + `GetObject` only, path-style endpoint) that the materializer uses for the S3 lane in place of the full AWS SDK; `ACP_STACK_S3_ENDPOINT_OVERRIDE` redirects it to a local mock for tests.

The Supabase logging sink runs as a single background task owned by `runtime::logging::supabase_sink::SupabaseSink`. Boot-time wiring in `cli::serve` resolves the service-role secret only when `[logging.supabase].enabled = true`, flips `StateStore::external_logging_enabled` on, and spawns the worker. Every persist call site that runs while the flag is on enqueues a `sink_outbox` row inside the same transaction that writes the source row, so local SQLite writes are atomic with delivery intent. The worker polls the outbox on a 1–30s exponential interval, groups rows by source table, hydrates each row through `state::sink_outbox::hydrate_*`, runs the per-table allowlist in `runtime::logging::sink_redaction`, and POSTs the redacted batch to `{url}/rest/v1/{table}` with `Prefer: resolution=merge-duplicates,return=minimal` for idempotent replay. Permanent 4xx responses park retries 24h out; 5xx/429/network failures back off with jitter. Failures fold into `sink_failures_summary` every 60s so `security::check` can surface a `logging.supabase.delivery_failing` finding without scanning the outbox. The Postgres dialect of every shared migration ships in `migrations/*.postgres.sql` and the analytics view layer (`session_turns`, `permissions`, `agent_events`, `security_events`, `connection_events`, `usage_metrics`) is authored alongside migration 007 for Supabase's consumption.

`AgentSupervisor` also owns the in-flight prompt registry (`HashMap<PromptId, PromptHandle>`). Each `POST /v1/sessions/{id}/prompt` enqueues a fire-and-forget background task that drives ACP `session/prompt` to completion and writes a terminal row into the `prompts` table; `session/cancel` fires the per-prompt `CancellationToken`. `acp_bridge` retains a cloneable `ConnectionTo<Agent>` handle once `initialize` completes so session dispatchers can call `session/new`, `session/load`, `session/resume`, `session/close`, `session/prompt`, and `session/cancel` without holding the supervisor's state lock across the agent's response. Incoming `session/update` notifications are persisted into `events` keyed by `session_id` via a `SessionEventSink` trait, then published live through the `events` broadcast hub to `/v1/ws` subscribers on `sessions.{session_id}`. SQLite remains the durable history source; WebSocket fanout is live only, with current producers for sessions, commands, workspace mutations, agent lifecycle, runtime status, and generic logs.

### Config vs State

The config describes what the runtime should be.

SQLite records what happened.

The age secret store contains secret values.

These three layers must remain separate:

- `acp-stack.toml` - portable desired environment
- `state.sqlite` - instance-local sessions, events, command runs, permission decisions, and lifecycle data
- `secrets.age` plus `age.key` - instance-local secret values and decrypt key

## Runtime Boundaries

- The runtime is a single Rust binary that supervises one configured ACP agent per runtime.
- The daemon, agent, MCP servers, and mediated commands run as the unprivileged runtime user by default.
- Config describes desired state, SQLite records runtime history, and the age-backed store holds secret values.
- External telemetry sinks consume the same normalized event stream as local SQLite logging.
