# Runtime

The runtime starts from config, prepares local state and secrets, launches the configured ACP agent, and exposes the public API/WebSocket surface plus an internal local socket for keyless local `acps` routes.

## Supervisor

The supervisor owns the configured agent process. It starts the agent with:

- the configured command, args, cwd, and restart policy
- a scrubbed environment with managed `PATH` and the runtime user's `HOME`; `[agent].env` cannot override these reserved keys
- secret values listed in `[agent].env` and each selected active-provider credential bundle

When `[workspace.sandbox]` is enabled, the agent process and mediated shells are launched inside an isolation backend that masks the daemon's secrets, config, state, and control socket from the workload. The backend is the same user as the daemon, so the managed `HOME`, `PATH`, and workspace are unchanged; only the runtime's own sensitive paths are hidden. See [security.md#sandbox](security.md#sandbox).

### Network-isolated spawns

With a `network-provider` extension declared (unshare backend only; see [extensions.md](extensions.md)), each wrapped spawn is supervised by the hidden `acps __sandbox-supervise` process, which the daemon spawns in place of the direct `unshare` chain. With no declared instance, host networking keeps the direct chain unchanged. The supervisor:

1. Creates a private sync socketpair and spawns the existing `unshare` chain with `--net` added, injecting the runtime-only `--sync-fd` into the in-namespace `__sandbox-exec` helper.
2. Waits for the helper's readiness byte (sent after tmpfs masking, before privilege drop), which proves the namespaces exist, then opens `/proc/<unshare-pid>/ns/net` and holds the fd for the whole spawn — this keeps the namespace alive without bind mounts and stays valid through teardown. It also opens and revalidates a pidfd for the blocked helper; later direct workload signals use that stable handle rather than a reusable numeric PID.
3. Runs provider `setup` beneath an internal process-group monitor and writes the release byte only on exit 0; the helper otherwise sees EOF and exits without ever executing the workload (fail-closed, including timeout). A private liveness socket ties the monitor to the sandbox supervisor, so supervisor death kills the complete in-contract provider process group even under SIGKILL.
4. Waits for the workload with stdio passed through untouched (the ACP transport for agent spawns). SIGINT/SIGTERM are forwarded to the workload directly as well as to `unshare` (which ignores them while waiting for its child), and the first forwarded signal arms a short grace window that escalates to SIGKILL on the chain, so shutdown can never hang. Teardown then runs while the namespace fd is still open. A shutdown signal arriving during teardown does not abort it — a cut-short teardown would guarantee a host-side resource leak — so teardown is bounded by the provider timeout only, and the supervisor still exits with the workload's already-recorded status. The chain itself carries `PR_SET_PDEATHSIG`, so even a SIGKILL of the supervisor cannot orphan `unshare` or the workload.
5. Mirrors the workload's exit code or signal exactly. A teardown failure after a successful workload exits with code 121; setup failure exits with code 120; a failed workload keeps its own status with the teardown error reported on the diagnostic channel. SIGKILL of the supervisor closes the namespace fd, triggers the provider monitor's group kill, and kills `unshare`; `--kill-child` then reaps the workload. Provider host-side resources that are not tied to namespace or process destruction are reconciled by the provider itself.

The supervisor's diagnostics (and provider stderr in `daemon` mode) are written to a daemon-stderr fd the spawn sites install at fd 3, so they never enter a mediated command's captured output. Namespace lifetime is strictly per wrapped spawn; there is no session- or workspace-scoped namespace registry. Provider contract and configuration are specified in [security.md#network-isolation-unshare-only](security.md#network-isolation-unshare-only).

Lifecycle transitions are recorded in durable state and published to live subscribers. After a successful spawn, the supervisor retains a sanitized provider/alias/revision snapshot for restart detection and clears it on stop or exit. Agent start, stop, and restart are admin operations. With `restart = "on-crash"`, an unexpected ACP subprocess or connection exit records `agent.exited`, schedules a bounded restart, and relaunches with the same resolved config and environment used for the prior successful start. `restart = "never"` leaves the process stopped. Planned stop, restart, and daemon shutdown do not trigger crash recovery.

Session requests that need the ACP bridge (`create`, `load`, `resume`, `fork`, `prompt`) start the configured agent themselves when the supervisor is stopped, so a host that has only ever run `acps serve` does not require an out-of-band `POST /v1/agent/start`. The same path brings the agent back after a crash. A request arriving during an in-flight start waits for it to settle instead of spawning a second agent; a supervisor that is stopping or updating, and any target configured with `restart = "never"`, still answer `agent.not_running`. Configuration, secret, and initialize failures surface unchanged.

Readiness scans recent `agent.started` lifecycle rows for live Unix process groups whose PID is not the currently supervised process. A match is reported as an orphaned agent or adapter process group and degrades `/v1/health/ready`.

Session recovery remains explicit. After an automatic relaunch, clients use `GET /v1/sessions/{id}/snapshot` to recover local state and call `POST /v1/sessions/{id}/resume` only when the agent advertises `sessionCapabilities.resume`.

## Agent Installation

Supported agents are declared in the embedded catalog. Entries may be native ACP agents or adapter-backed agents; adapter entries either install separate harness and adapter steps or declare that the harness is provided by the adapter package.

Installer behavior:

- refuse unsupported catalog entries
- install into runtime-managed paths
- verify declared executables after install
- record install outcomes for operator inspection
- never receive provider API keys

Pinned installs use catalog metadata when available. Floating installs use the catalog's preferred install path.

Each install field walks its declared paths in priority order (shell → npm → github_release floating; github → npm pinned), recording every attempt. When several paths fail or are skipped, the surfaced error enumerates each path with its own failure (`shell: …; npm: …; github: …`) instead of reporting only the last path tried. Verifying an installed executable — each path's result and the final `[agent].command` check — goes beyond resolving it on PATH: the file must carry an executable format (ELF, Mach-O, or a `#!` interpreter line) and survive a `--version` spawn probe that judges spawnability only, so a stub left by a blocked package postinstall or a wrong-architecture download fails that path and the chain advances. With a declared `expected_sha256` pin, step-level verification is format-header-only — a wrong-architecture binary then fails the whole install in final verification rather than failing its path — and the `--version` spawn probe runs once, after the pin check, in final verification, so a binary that fails the operator's pin is never executed. The probe runs with the same scrubbed installer environment (never provider API keys) and only after any declared `expected_sha256` or asset checksum has been verified. A pre-existing binary that fails the same gate reads as absent instead of short-circuiting the install (including on resumed `agent_install` steps) and is reinstalled; one that fails its integrity pin is never executed — it errors on the spot, or, on a resumed step, reads as absent so the reinstall can surface a still-mismatching pin in final verification.

Install steps run with a scrubbed environment: PATH (plus the destination directory), HOME, LANG, the non-interactive hints, and `[agent].env` values, which may not override any of the former. One addition is computed rather than forwarded — `npm_config_python` is set to the path `python3` reports as its own executable. node-gyp spawns `python3` once per native module it configures, so on a host where that name is a version-manager shim rather than a binary, every one of those spawns pays the shim's cost; a package tree with many native modules can then consume an entire install budget without a compiler ever running. Resolving it once costs a single shim invocation, and the variable is inert where `python3` is already a binary. A registry entry that needs a different interpreter can still override it through `[agent].env`.

Install-flow requests to `api.github.com` (release resolution and asset download, including `acps agent check`) share a process-wide pace: a minimum interval between requests, and a cooldown circuit that opens when GitHub answers with a rate-limit response (429, or 403 with an exhausted quota or `Retry-After` header), honoring `Retry-After`/`X-RateLimit-Reset`. Waits beyond a hard cap surface as a typed rate-limit error instead of blocking indefinitely. This keeps install retry loops on shared-egress-IP hosts from burning the unauthenticated GitHub quota. `acps update` does not go through this pacing.

## Init

`acps init` creates or validates config and state, initializes encrypted secrets, generates API keys when absent, and can configure agents (registry or custom), Agent Skills, providers, workspace sources, MCP servers, extra agent environment variables, dependency install actions, edge profiles, and testflight. The full operator-facing sequence is documented in [init.md](init.md).

Init is resumable. A resumed run skips completed work whose result still exists and retries incomplete or failed work. Existing config and API keys are preserved unless the operator explicitly resets the instance.

## Provider And Model Resolution

Provider ids are resolved through the provider metadata for the configured agent. Starts, restarts, installs, model discovery, and agent tests share one environment resolver for generic refs and provider bundles. Equal values for a shared env name are deduplicated; different values fail without exposing them. Mapped models and modes are validated against ACP-advertised session config options where the agent exposes them.

A catalog credential that covers an env var wins over a bare `[agent].env` ref of the same name: the bare ref is skipped and the catalog value is injected, so a managed rotation takes effect without touching the flat store. Templated `VAR=...` entries keep flat-store semantics. Custom providers inject their configured `api_key_ref` from the catalog credential stored under their provider id, falling back to the flat store; when neither store holds the key, agent start fails with an error naming the provider and ref.

Custom providers are accepted only for agents that support them. Custom model ids are operator-supplied and are not certified by `acp-stack`.

Agent-owned config files are written before canonical config changes are committed. If provisioning fails, the canonical config is not advanced.

## Native Agent-Config Import

Supported Agent user-global configs are imported as configuration meaning rather than byte-preserving copies. Inspection removes every `acps`-managed or `acps`-controlled path from the native residual. Compatible selected provider, model, and MCP candidates pass through the canonical config validation and secret-reference paths; compatible candidates left unselected keep the current canonical value. Credentials, login state, permission and sandbox controls, and managed fields without a lossless mapping remain blocked. The unmanaged residual replaces the prior unmanaged settings, then the normal headless provisioner overlays canonical managed values.

Existing-instance apply is serialized through the agent-config mutation file lock, held by native config import/cancel, `acps agent set`, agent lifecycle transitions, config import, and hosted-init apply; other config writers are not serialized. Apply checks restart blockers before touching live files. A blocked import stages the entire transaction in the owner-only operation journal. Immediate apply stops the running primary agent when needed, snapshots canonical and native files, writes them atomically, refreshes live state, and restores the prior files and process on failure. Pending apply and rollback phases are resumed from the journal after daemon restart.

Terminal operations stay queryable for 24 hours, then journal and in-memory state are pruned. Cancel-of-applied rollback expires after 15 minutes; cancellation after expiry returns `agent.native_config_rollback_expired`.

## Workspace And Files

The workspace API is rooted at `[workspace].root`. All request paths are workspace-relative. The runtime rejects traversal, absolute paths, NUL bytes, and symlink escapes.

Workspace operations support:

- metadata
- directory listing
- file read/write
- upload/download
- single-file delete

Writes are atomic where supported by the host filesystem. Mutations are logged and published to the workspace event topic.

## Workspace Sources

Workspace sources populate a new or empty destination under the workspace:

- Git code sources under `usr/code`
- local, HTTPS, or S3 data sources under `usr/data`

Materialization refuses unsafe archives, parent-directory traversal, symlinks, hardlinks, special files, and oversized entries. Each completed source drops a `.acp-stack-source.json` sentinel at its destination root. A non-empty destination without a matching sentinel hard-fails with `workspace.destination_not_empty` so init never silently merges into existing content.

## Command Gateway

The Command Gateway runs shell commands through the configured default shell inside the workspace boundary. It applies permission policy before execution, streams output to live subscribers, persists bounded output, and supports cancellation and timeouts. On Unix, the validated CWD is rebound through a verified directory handle at spawn time.

Persisted output chunks are command-scoped events with stream name, sequence number, timestamp, event id, and command id. Command rows track the latest output event, output byte count, and latest progress timestamp so clients can reconnect and distinguish quiet work from a stalled runtime. Durable command state writes must succeed before side effects continue; volatile WebSocket fanout remains best effort.

While a command is running, the gateway emits `command.progress` events every `[commands].progress_interval` when no output has reset the quiet timer. Cancellation produces a terminal `command.cancelled` event after the child process is settled.

A command that reaches a terminal status while its permission request is still pending cancels that permission with a reason naming the cause (see the reason table in `docs/specs/state-logging.md`); a permission request never outlives its command. The startup command sweep upholds the same invariant by canceling dependent pending permissions in the transaction that fails the orphaned commands.

Only environment variables in `[commands].env_allowlist` are forwarded from the request. Secrets are not injected into command children unless another explicit runtime mechanism provides them.

## Prompts

### Stale-Prompt Sweeper

A background task flips `pending`/`running` prompt rows to terminal `stalled` when no ACP `session/update` notification has touched the row within the configured threshold. Without it, an agent that crashes mid-stream or hangs on an upstream call would leave rows stuck in `running` forever, breaking client polling.

Config under `[prompts]`:

```toml
[prompts]
stale_threshold = "5m"
sweep_interval  = "30s"
```

Defaults are `5m` / `30s`. The sweeper runs every `sweep_interval` from `acps serve`; the first sweep happens after one interval has elapsed (not immediately at boot) so startup reconcile settles first. `stalled` is terminal: a flipped row does not transition back, and recovery means submitting a fresh prompt. Each flipped row also emits a `prompt.stalled` session event (see `docs/specs/state-logging.md`).

Re-touch path: every ACP `session/update` runs through `touch_running_prompt` (in `src/runtime/agent/session_sink.rs`), which advances `updated_at` on the oldest in-flight prompt for that session. ACP notifications carry no `prompt_id`, so the session-scoped lookup is the best precision available; concurrent multi-prompt sessions are not currently supported through this path. The `PromptsHealth` probe surfaced through `/v1/health/ready` and `acps status` reports the stuck-prompt count using the same threshold, so operators see stalled traffic before the next sweep cadence.

### Inference Failure Classifier

When the agent's `session/prompt` call fails because the underlying inference provider returned an HTTP error, the SDK surfaces it as an ACP error whose `Display` output embeds the upstream status text. The classifier (`src/runtime/agent/inference_failure.rs`) sits between the SDK error and the persisted prompt row, deciding whether the failure is `inference_5xx`, `inference_4xx`, or generic `agent_request`.

Sanitization contract: the classifier returns a `Classified { class, status_code: Option<u16>, reason_category: &'static str }`. Only the enum variant, the parsed `u16`, and a `&'static str` drawn from a fixed catalog can flow out — the raw upstream message never reaches state, events, or API responses. Callers persist `reason_category` directly into `prompts.failure_detail_json` and the `prompt.inference_failed` event payload.

The `reason_category` catalog is:

- `rate_limit` (HTTP 429)
- `internal_server_error` (HTTP 500)
- `bad_gateway` (HTTP 502)
- `service_unavailable` (HTTP 503)
- `gateway_timeout` (HTTP 504)
- `server_overloaded` (HTTP 529)
- `client_error` (any other 4xx)
- `unknown` (no status code parsed)

500-range codes plus the 529-overloaded variant map to `FailureClass::Inference5xx` and HTTP 502; 400-range codes map to `FailureClass::Inference4xx` and HTTP 424. Anything else falls back to `FailureClass::AgentRequest` with `reason_category = "unknown"`.

## Dependencies And MCP

Dependency declarations report whether expected tools, packages, runtimes, and MCP servers are present. Commands and runtimes are checked as executables on PATH. Package checks use local Linux package databases when available. Install actions run only when explicitly declared for a command dependency.

MCP server declarations are resolved at ACP session creation, load, or resume. Secret refs for stdio env vars and HTTP headers are resolved from the encrypted secret store at attach time.

Readiness reports MCP declaration health. Stdio declarations check executable command availability and referenced secrets; HTTP declarations check referenced secrets without probing remote endpoints.

## Self-Hosting

The supported deployment shapes are Docker and systemd. Public exposure should go through Cloudflare Tunnel, Nginx, or Caddy while runtime hardening remains enabled behind the edge.
