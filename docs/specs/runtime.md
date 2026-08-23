# Runtime

The runtime starts from config, prepares local state and secrets, and launches the configured ACP agent. It exposes two surfaces: the public API/WebSocket, and an internal local socket for keyless local `acps` routes.

## Supervisor

The supervisor owns the configured agent process. It starts the agent with:

- the configured command, args, cwd, and restart policy
- a scrubbed environment with managed `PATH` and the runtime user's `HOME`; `[agent].env` cannot override these reserved keys
- secret values listed in `[agent].env` and each selected active-provider credential bundle

When `[workspace.sandbox]` is enabled, the agent process and mediated shells launch inside an isolation backend. The backend masks the daemon's secrets, config, state, and control socket from the workload. It runs as the same user as the daemon, so the managed `HOME`, `PATH`, and workspace are unchanged; only the runtime's own sensitive paths are hidden. See [security.md#sandbox](security.md#sandbox).

### Network-isolated spawns

With a `network-provider` extension declared (unshare backend only; see [extensions.md](extensions.md#type-network-provider)), each wrapped spawn is supervised by the hidden `acps __sandbox-supervise` process. The daemon spawns it in place of the direct `unshare` chain. With no declared instance, host networking keeps the direct chain unchanged.

The supervisor:

1. Creates a private sync socketpair. Spawns the existing `unshare` chain with `--net` added. Injects the runtime-only `--sync-fd` into the in-namespace `__sandbox-exec` helper.
2. Waits for the helper's readiness byte, sent after tmpfs masking and before the privilege drop. The byte proves the namespaces exist. It then opens `/proc/<unshare-pid>/ns/net` and holds the fd for the whole spawn. This keeps the namespace alive without bind mounts and stays valid through teardown. It also opens and revalidates a pidfd for the blocked helper. Later direct workload signals use that stable handle rather than a reusable numeric PID.
3. Runs provider `setup` beneath an internal process-group monitor. It writes the release byte only on exit 0. Otherwise the helper sees EOF and exits without ever executing the workload — fail-closed, including timeout. A private liveness socket ties the monitor to the sandbox supervisor. Supervisor death kills the complete in-contract provider process group, even under SIGKILL.
4. Waits for the workload with stdio passed through untouched. For agent spawns, that stdio is the ACP transport. Signal and teardown handling:
    - SIGINT/SIGTERM are forwarded to the workload directly, and also to `unshare`, which ignores them while waiting for its child.
    - The first forwarded signal arms a short grace window that escalates to SIGKILL on the chain, so shutdown can never hang.
    - Teardown runs while the namespace fd is still open. A shutdown signal arriving during teardown does not abort it: a cut-short teardown would guarantee a host-side resource leak. Teardown is bounded by the provider timeout only. The supervisor still exits with the workload's already-recorded status.
    - The chain carries `PR_SET_PDEATHSIG`, so even a SIGKILL of the supervisor cannot orphan `unshare` or the workload.
5. Mirrors the workload's exit code or signal exactly:
    - A teardown failure after a successful workload exits with code 121.
    - A setup failure exits with code 120.
    - A failed workload keeps its own status, with the teardown error reported on the diagnostic channel.
    - SIGKILL of the supervisor closes the namespace fd, triggers the provider monitor's group kill, and kills `unshare`; `--kill-child` then reaps the workload.
    - Provider host-side resources not tied to namespace or process destruction are reconciled by the provider itself.

#### Diagnostics And Scope

- The supervisor's diagnostics, and provider stderr in `daemon` mode, are written to a daemon-stderr fd the spawn sites install at fd 3. They never enter a mediated command's captured output.
- Namespace lifetime is strictly per wrapped spawn. There is no session- or workspace-scoped namespace registry.
- The provider contract and configuration are specified in [security.md](security.md#network-isolation-unshare-only).

### Lifecycle

- Transitions are recorded in durable state and published to live subscribers.
- After a successful spawn, the supervisor retains a sanitized provider/alias/revision snapshot for restart detection. It clears the snapshot on stop or exit.
- Agent start, stop, and restart are admin operations.
- With `restart = "on-crash"`, an unexpected ACP subprocess or connection exit records `agent.exited`, schedules a bounded restart, and relaunches with the same resolved config and environment used for the prior successful start.
- `restart = "never"` leaves the process stopped.
- Planned stop, restart, and daemon shutdown do not trigger crash recovery.

### Automatic Start On Session Requests

- Session requests that need the ACP bridge (`create`, `load`, `resume`, `fork`, `prompt`) start the configured agent themselves when the supervisor is stopped. A host that has only ever run `acps serve` needs no out-of-band `POST /v1/agent/start`.
- The same path brings the agent back after a crash.
- A request arriving during an in-flight start waits for it to settle instead of spawning a second agent.
- A supervisor that is stopping or updating, and any target configured with `restart = "never"`, still answer `agent.not_running`.
- Configuration, secret, and initialize failures surface unchanged.

### Readiness

- Readiness scans recent `agent.started` lifecycle rows for live Unix process groups whose PID is not the currently supervised process.
- A match is reported as an orphaned agent or adapter process group and degrades `/v1/health/ready`.

Session recovery remains explicit. After an automatic relaunch, clients use `GET /v1/sessions/{id}/snapshot` to recover local state. They call `POST /v1/sessions/{id}/resume` only when the agent advertises `sessionCapabilities.resume` (see [acp-bridge.md](acp/acp-bridge.md)).

## Agent Installation

Supported agents are declared in the embedded catalog. Entries may be native ACP agents or adapter-backed agents. Adapter entries either install separate harness and adapter steps or declare that the harness is provided by the adapter package.

An operator `[agent.adapter_override]` block makes the effective entry adapter-backed regardless of catalog kind:

- The harness keeps its catalog install/update under the `harness` step.
- The operator's adapter installs under the `adapter` step.
- npm/github override installs join managed update.
- Shell-only overrides are skipped unless `update.shell_rerun` is set.

Adding an override to an agent first installed as a native entry relabels its expected steps. `acps agent check` then reports the harness not installed until the next install or update re-runs the idempotent recipe.

### Installer Behavior

- refuse unsupported catalog entries
- install into runtime-managed paths
- verify declared executables after install
- record install outcomes for operator inspection
- never receive provider API keys

Pinned installs use catalog metadata when available. Floating installs use the catalog's preferred install path.

### Path Fallback

- Each install field walks its declared paths in priority order: shell → npm → github_release when floating; github → npm when pinned.
- Every attempt is recorded.
- When several paths fail or are skipped, the surfaced error enumerates each path with its own failure (`shell: …; npm: …; github: …`) instead of reporting only the last path tried.

Executable verification applies to each path's result and to the final `[agent].command` check. It goes beyond resolving the file on PATH:

- The file must carry an executable format: ELF, Mach-O, or a `#!` interpreter line.
- It must survive a `--version` spawn probe that judges spawnability only.

A stub left by a blocked package postinstall or a wrong-architecture download fails that path, and the chain advances. The probe runs with the same scrubbed installer environment (provider API keys stay out). It runs only after any declared `expected_sha256` or asset checksum has been verified.

With a declared `expected_sha256` pin:

- Step-level verification is format-header-only. A wrong-architecture binary then fails the whole install in final verification rather than failing its path.
- The `--version` spawn probe runs once, after the pin check, in final verification. Only a binary that passes the operator's pin is executed.

Pre-existing binaries follow the same gate:

- One that fails the gate reads as absent — including on resumed `agent_install` steps — and is reinstalled.
- One that fails its integrity pin is refused execution. It errors on the spot, or, on a resumed step, reads as absent so the reinstall can surface a still-mismatching pin in final verification.

### Install Environment

Install steps run with a scrubbed environment:

- `PATH` (plus the destination directory), `HOME`, `LANG`, and the non-interactive hints
- `[agent].env` values, with the former keys reserved

Every variable above is forwarded; one more is computed on the spot: `npm_config_python` is set to the path `python3` reports as its own executable, so node-gyp's repeated `python3` spawns bypass version-manager shims. A registry entry that needs a different interpreter can still override it through `[agent].env`.

### GitHub Request Pacing

- Install-flow requests to `api.github.com` — release resolution and asset download, including `acps agent check` — share a process-wide pace with a minimum interval between requests.
- A cooldown circuit opens when GitHub answers with a rate-limit response: 429, or 403 with an exhausted quota or a `Retry-After` header. The circuit honors `Retry-After`/`X-RateLimit-Reset`.
- Waits beyond a hard cap surface as a typed rate-limit error instead of blocking indefinitely.
- This keeps install retry loops on shared-egress-IP hosts from burning the unauthenticated GitHub quota.
- `acps update` does not go through this pacing.

## Init

`acps init` creates or validates config and state, initializes encrypted secrets, and generates API keys when absent. The full operator-facing sequence — agents, skills, providers, workspace sources, MCP servers, agent environment, dependency install actions, edge profiles, testflight — is documented in [init.md](init.md) under Flow.

The init step machine is resumable:

- A resumed run skips completed work whose result still exists.
- Incomplete or failed work is retried.
- Existing config and API keys are preserved unless the operator explicitly resets the instance.

## Provider And Model Resolution

Provider ids are resolved through the provider metadata for the configured agent.

- Starts, restarts, installs, model discovery, and agent tests share one environment resolver for generic refs and provider bundles.
- Equal values for a shared env name are deduplicated; different values fail without exposing them.
- Mapped models and modes are validated against ACP-advertised session config options where the agent exposes them.

### Credential Precedence

- A catalog credential that covers an env var wins over a bare `[agent].env` ref of the same name. The bare ref is skipped and the catalog value is injected, so a managed rotation takes effect without touching the flat store.
- Templated `VAR=...` entries keep flat-store semantics.
- Custom providers inject their configured `api_key_ref` from the catalog credential stored under their provider id, falling back to the flat store.
- When the key is absent from both stores, agent start fails with an error naming the provider and ref.

Custom providers are accepted only for agents that support them. Custom model ids are operator-supplied and remain outside `acp-stack` certification.

Agent-owned config files are written before canonical config changes are committed. If provisioning fails, the canonical config keeps its last committed state.

## Native Agent-Config Import

Supported Agent user-global configs are imported as configuration meaning, not byte-preserving copies.

- Inspection removes every `acps`-managed or `acps`-controlled path from the native residual.
- Compatible selected provider, model, and MCP candidates pass through the canonical config validation and secret-reference paths.
- Compatible candidates left unselected keep the current canonical value.
- Credentials, login state, permission and sandbox controls, and managed fields whose mapping would lose information remain blocked.
- The unmanaged residual replaces the prior unmanaged settings. The normal headless provisioner then overlays canonical managed values.

Existing-instance apply is serialized through the agent-config mutation file lock. The lock is held by native config import/cancel, `acps agent set`, agent lifecycle transitions, config import, and hosted-init apply. Other config writers proceed independently of the lock.

### Apply Behavior

- Apply checks restart blockers before touching live files.
- A blocked import stages the entire transaction in the owner-only operation journal.
- Immediate apply stops the running primary agent when needed, snapshots canonical and native files, writes them atomically, and refreshes live state. On failure it restores the prior files and process.
- Pending apply and rollback phases are resumed from the journal after daemon restart.

### Journal Retention

- Terminal operations stay queryable for 24 hours, then journal and in-memory state are pruned.
- Cancel-of-applied rollback expires after 15 minutes. Cancellation after expiry returns `agent.native_config_rollback_expired`.

## Workspace And Files

The workspace API is rooted at `[workspace].root`. All request paths are workspace-relative. The runtime rejects:

- traversal
- absolute paths
- NUL bytes
- symlink escapes

Workspace operations support:

- metadata
- directory listing
- file read/write
- upload/download
- single-file delete

Writes are atomic where the host filesystem supports it. Mutations are logged and published to the workspace event topic.

## Workspace Sources

Workspace sources populate a new or empty destination under the workspace:

- Git code sources under `usr/code`
- local, HTTPS, or S3 data sources under `usr/data`

Materialization refuses:

- unsafe archives
- parent-directory traversal
- symlinks and hardlinks
- special files
- oversized entries

Each completed source drops a `.acp-stack-source.json` sentinel at its destination root. Init merges into existing content only when that sentinel matches; any other non-empty destination hard-fails with `workspace.destination_not_empty`.

## Command Gateway

The Command Gateway runs shell commands through the configured default shell inside the workspace boundary. It:

- applies permission policy before execution
- streams output to live subscribers
- persists bounded output
- supports cancellation and timeouts

On Unix, the validated CWD is rebound through a verified directory handle at spawn time.

### Persisted Output

- Chunks are command-scoped events with stream name, sequence number, timestamp, event id, and command id.
- Command rows track the latest output event, output byte count, and latest progress timestamp, so clients can reconnect and distinguish quiet work from a stalled runtime.
- Durable command state writes must succeed before side effects continue. Volatile WebSocket fanout remains best effort.

### Progress And Cancellation

- While a command is running, the gateway emits `command.progress` events every `[commands].progress_interval` when no output has reset the quiet timer.
- Cancellation produces a terminal `command.cancelled` event after the child process is settled.

### Permission Lifetime

- A command that reaches a terminal status while its permission request is still pending cancels that permission with a reason naming the cause (see the reason table in [state-logging.md](state-logging.md)).
- A permission request never outlives its command.
- The startup command sweep upholds the same invariant: it cancels dependent pending permissions in the transaction that fails the orphaned commands.

### Environment

- Only environment variables in `[commands].env_allowlist` are forwarded from the request.
- Secrets are not injected into command children unless another explicit runtime mechanism provides them.

## Prompts

### Stale-Prompt Sweeper

A background task flips `pending`/`running` prompt rows to terminal `stalled` when no ACP `session/update` notification has touched the row within the configured threshold, so an agent that crashed mid-stream or hung on an upstream call cannot leave rows in `running` forever.

Config under `[prompts]`:

```toml
[prompts]
stale_threshold = "5m"
sweep_interval  = "30s"
```

- Defaults are `5m` / `30s`.
- The sweeper runs every `sweep_interval` from `acps serve`. The first sweep happens after one interval has elapsed, not immediately at boot, so startup reconcile settles first.
- `stalled` is terminal: a flipped row does not transition back. Recovery means submitting a fresh prompt.
- Each flipped row also emits a `prompt.stalled` session event (see [state-logging.md](state-logging.md)).

#### Re-Touch Path

- Every ACP `session/update` advances `updated_at` on the oldest in-flight prompt for that session.
- ACP notifications carry no `prompt_id`, so the session-scoped lookup is the best precision available. Concurrent multi-prompt sessions are not currently supported through this path.
- The `PromptsHealth` probe surfaced through `/v1/health/ready` and `acps status` reports the stuck-prompt count using the same threshold, so operators see stalled traffic before the next sweep cadence.

### Inference Failure Classifier

The agent's `session/prompt` call can fail when the underlying inference provider returns an HTTP error. The SDK surfaces that as an ACP error whose `Display` output embeds the upstream status text. A classifier sits between the SDK error and the persisted prompt row and decides whether the failure is `inference_5xx`, `inference_4xx`, or generic `agent_request`.

#### Sanitization Contract

- The classifier returns `Classified { class, status_code: Option<u16>, reason_category: &'static str }`.
- Only the enum variant, the parsed `u16`, and a `&'static str` drawn from a fixed catalog can flow out. The raw upstream message never reaches state, events, or API responses.
- Callers persist `reason_category` directly into `prompts.failure_detail_json` and the `prompt.inference_failed` event payload.

The `reason_category` catalog is:

- `rate_limit` (HTTP 429)
- `internal_server_error` (HTTP 500)
- `bad_gateway` (HTTP 502)
- `service_unavailable` (HTTP 503)
- `gateway_timeout` (HTTP 504)
- `server_overloaded` (HTTP 529)
- `client_error` (any other 4xx)
- `unknown` (no status code parsed)

#### Class Mapping

- 500-range codes plus the 529-overloaded variant map to `FailureClass::Inference5xx` and HTTP 502.
- 400-range codes map to `FailureClass::Inference4xx` and HTTP 424.
- Anything else falls back to `FailureClass::AgentRequest` with `reason_category = "unknown"`.

## Dependencies And MCP

### Dependency Declarations

- Declarations report whether expected tools, packages, runtimes, and MCP servers are present.
- Commands and runtimes are checked as executables on PATH.
- Package checks use local Linux package databases when available.
- Install actions run only when explicitly declared for a command dependency.

### MCP Declarations

- MCP server declarations are resolved at ACP session creation, load, or resume.
- Secret refs for stdio env vars and HTTP headers are resolved from the encrypted secret store at attach time.

### Readiness

- Readiness reports MCP declaration health.
- Stdio declarations check executable command availability and referenced secrets.
- HTTP declarations check referenced secrets without probing remote endpoints.

## Self-Hosting

The supported deployment shapes are Docker and systemd. Public exposure should go through Cloudflare Tunnel, Nginx, or Caddy. Runtime hardening remains enabled behind the edge.
