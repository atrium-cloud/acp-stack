# Runtime

The runtime starts from config, prepares local state and secrets, launches the configured ACP agent, and exposes the API, WebSocket, and local `acpctl` interfaces.

## Supervisor

The supervisor owns the configured agent process. It starts the agent with:

- the configured command, args, cwd, and restart policy
- a scrubbed environment with managed `PATH` and the runtime user's `HOME`; `[agent].env` cannot override these reserved keys
- secret values listed in `[agent].env`

Lifecycle transitions are recorded in durable state and published to live subscribers. Agent start, stop, and restart are admin operations. With `restart = "on-crash"`, an unexpected ACP subprocess or connection exit records `agent.exited`, schedules a bounded restart, and relaunches with the same resolved config and environment used for the prior successful start. `restart = "never"` leaves the process stopped. Planned stop, restart, and daemon shutdown do not trigger crash recovery.

Session recovery remains explicit. After an automatic relaunch, clients use `GET /v1/sessions/{id}/snapshot` to recover local state and call `POST /v1/sessions/{id}/resume` only when the agent advertises `sessionCapabilities.resume`.

## Agent Installation

Supported agents are declared in the embedded catalog. Entries may be native ACP agents or adapter-backed agents with separate harness and adapter install steps.

Installer behavior:

- refuse unsupported catalog entries
- install into runtime-managed paths
- verify declared executables after install
- record install outcomes for operator inspection
- never receive provider API keys

Pinned installs use catalog metadata when available. Floating installs use the catalog's preferred install path.

## Init

`acps init` creates or validates config and state, initializes encrypted secrets, generates API keys when absent, and can configure agents, Agent Skills, providers, workspace sources, MCP servers, edge profiles, and testflight.

Init is resumable. A resumed run skips completed work whose result still exists and retries incomplete or failed work. Existing config and API keys are preserved unless the operator explicitly resets the instance.

## Provider And Model Resolution

Provider ids are resolved through the provider metadata for the configured agent. Mapped models and modes are validated against ACP-advertised session config options where the agent exposes them.

Custom providers are accepted only for agents that support them. Custom model ids are operator-supplied and are not certified by `acp-stack`.

Agent-owned config files are written before canonical config changes are committed. If provisioning fails, the canonical config is not advanced.

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

The Command Gateway runs shell commands through the configured default shell inside the workspace boundary. It applies permission policy before execution, streams output to live subscribers, persists bounded output, and supports cancellation and timeouts.

Persisted output chunks are command-scoped events with stream name, sequence number, timestamp, event id, and command id. Command rows track the latest output event, output byte count, and latest progress timestamp so clients can reconnect and distinguish quiet work from a stalled runtime.

While a command is running, the gateway emits `command.progress` events every `[commands].progress_interval` when no output has reset the quiet timer. Cancellation produces a terminal `command.canceled` event after the child process is settled.

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

Dependency declarations report whether expected tools, packages, runtimes, and MCP servers are present. Install actions run only when explicitly declared for a command dependency.

MCP server declarations are resolved at ACP session creation, load, or resume. Secret refs for stdio env vars and HTTP headers are resolved from the encrypted secret store at attach time.

Readiness reports MCP declaration health. Stdio declarations check executable command availability and referenced secrets; HTTP declarations check referenced secrets without probing remote endpoints.

## Self-Hosting

The supported deployment shapes are Docker and systemd. Public exposure should go through Cloudflare Tunnel, Nginx, or Caddy while runtime hardening remains enabled behind the edge.
