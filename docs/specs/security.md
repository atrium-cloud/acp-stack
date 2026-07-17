# Security

`acp-stack` treats local instance integrity as part of the product contract. The runtime fails fast on unsafe config and keeps secret values out of config, responses, and logs.

## API Keys

Two API keys are generated on first init:

| Key     | Scope                                                                   |
| ------- | ----------------------------------------------------------------------- |
| Session | public session-tier API calls, including session lifecycle and prompts   |
| Admin   | secrets, config import, agent process control, and sensitive operations |

The session key can be regenerated. The admin key is generated once and is replaced only by resetting and reinitializing the instance.

Plaintext auth keys are printed only at init or session-key regeneration time. Local state stores non-recoverable verifier rows for the session and admin keys; config and `secrets.age` do not store auth keys.

`acps init serve` uses a separate bootstrap bearer token supplied by environment variable or file. That token is never persisted and is valid only for the bootstrap init routes. Hosted init status and event replay omit plaintext auth keys; keys appear only in the explicit WebSocket result handoff and are cleared from memory after `ack_result`.

## Key Tiering

Tiering is strict and non-superset: the admin key is rejected on session-tier routes with `401 auth.wrong_kind`, and the session key is rejected on admin-tier routes with the same code. The admin key is not a superset of the session key.

Session-tier routes cover public API operations that are not management or destructive: config export, workspace operations, command runs, session and prompt lifecycle, and permission approve/deny. Session operations stay session-tier even when they write rows.

`[local].session_auth = "keyless"` is a local Unix-socket exception, not a public API tier change. When enabled, same-user local callers can reach session-tier HTTP routes without bearer auth through `acps`; public HTTP routes still require the session key and still reject the admin key.

Both keys are presented as `Authorization: Bearer <key>` and validated against stored verifiers in constant time.

## Secret Store

Secret values are stored in the encrypted local secret store. Config files carry secret reference names only.

The same `secrets.age` ciphertext contains the instance-wide mapped-provider credential catalog. A provider has either one aliasless credential or a permanently promoted alias map. Each credential bundle contains its required and supplied env fields, retained source-ref names, and an opaque revision used only to detect whether a running process is stale.

Rules:

- API responses never return secret values.
- Provider status and credential-list output never return credential values or revisions.
- Config export returns refs only.
- Agent and MCP secrets are injected only where explicitly referenced.
- Secret-ref fields reject likely pasted secret values.
- Auth keys are not secret-store entries.

## HTTP Hardening

The public API enforces:

- bearer authentication
- auth tier checks
- CORS and WebSocket Origin allowlists
- request body limits
- per-key and per-IP rate limits
- temporary blocking after repeated auth failures
- bounded trusted-proxy handling
- security event logging

`trust_proxy_headers = true` accepts forwarded client metadata only from exact IPs listed in `trusted_proxies`. Do not trust broad public ranges.

## Auth Failures And Rate Limits

Every rejected authentication is recorded in the `auth_failures` table with a structural `reason` (`missing`, `malformed_header`, `invalid`, `wrong_kind`). The attempted token value is never stored. After `auth_failures_per_minute` rejections in a 60-second window, the client IP is blocked for `auth_block_duration`.

Hardening errors use stable codes in the standard error envelope:

| Status | Code                       | Trigger                                          |
| ------ | -------------------------- | ------------------------------------------------ |
| `401`  | `auth.wrong_kind`          | key valid but used on the wrong tier             |
| `429`  | `auth.rate_limited`        | per-IP, per-key, or unauthenticated bucket empty |
| `403`  | `auth.origin_not_allowed`  | CORS / WebSocket Origin not in allowlist         |
| `413`  | `request.too_large`        | request body exceeds the configured cap          |

## Permissions

Permission policy applies to ACP permission requests and mediated shell commands.

| Policy input      | Behavior                                   |
| ----------------- | ------------------------------------------ |
| `deny` match      | reject immediately                         |
| `review` match    | create a permission request or audit event |
| `auto` mode       | allow unmatched requests                   |
| `supervised` mode | require approval for unmatched risky work  |
| `locked` mode     | require approval for unmatched commands    |

Shell command policy matches both raw and shell-word-normalized command forms. Constructed command words require review when no deny or review pattern matches.

Pending requests expire according to config. Approval and denial decisions are durable events.

### ACP client terminals

`terminal/create` executes directly, without a permission-service gate: the VM is the security boundary, and agents send `session/request_permission` separately when their own policy requires review. Compensating controls: every terminal is recorded as an `acp`-origin row in the durable command log with its output and exit status; terminal children inherit the same `[workspace.sandbox]` profile as the agent harness; their working directory is confined to the session workspace; and they receive a clean session environment — managed `PATH` and `HOME` plus the agent-supplied vars, never the `[agent].env` provider secrets injected into the agent process itself.

## Workspace Boundary

Workspace paths are resolved under `[workspace].root`. The runtime rejects absolute paths from API callers, `..` traversal, embedded NUL bytes, symlink escapes, writes through existing symlink targets, and files above `workspace.max_file_bytes`. Oversized reads/writes/uploads/downloads return `413 workspace.too_large`.

ACP `fs/read_text_file` and `fs/write_text_file` requests carry absolute paths by protocol; those are accepted only when they resolve inside the session workspace through the same canonicalization and symlink refusal, and each write records a durable `fs.write` audit event.

## Local Interface

Keyless local `acps` views use a local Unix socket protected by filesystem permissions. Low-risk observability routes are always available without public session or admin API keys. When `[local].session_auth = "keyless"` is enabled by an admin, same-user local callers can also reach session-tier HTTP routes without bearer auth. Admin-tier operations remain unavailable on the local socket: callers cannot read secret values, rotate keys, import config, apply dependencies, or control public WebSocket disconnections locally.

## Native Config Import

Native Agent-config inspection and mutation are admin-tier operations. Inspection manifests contain paths and classifications, never field values, commands, headers, credentials, or login state. Uploaded content is capped at 1 MiB; the raw document exists only in the process-local, revision-bound inspection draft. A selected import durably stages only the prepared canonical config and stripped native residual. Hosted init does not place the raw document in persisted init arguments, events, progress, or handoff metadata.

Credentials, authentication state, permission and sandbox controls, and other `acps`-owned security fields are removed before an unmanaged residual can be written. Unmanaged hooks, notification commands, command helpers, plugins, or formatters require explicit acknowledgement for the inspected SHA-256 revision. Transaction targets are fixed under the runtime user's home, must pass ownership and regular-file checks, and reject symlinks and linked files; managed directories and files use owner-only permissions.

## Deployment Posture

Production deployments should:

- run as an unprivileged runtime user
- configure `[workspace].runtime_user` to a local user that resolves on the host
- keep config and state directories owner-only
- bind the daemon to loopback unless a trusted platform requires otherwise
- terminate TLS at a reverse proxy or Cloudflare Tunnel
- keep runtime auth and origin checks enabled behind the edge

## Sandbox

By default the agent harness and mediated shells run in the same process tree and OS user as the daemon, which holds the runtime's secrets, config, and control socket. For an untrusted workload this means in-runtime policy is bypassable and the on-disk secrets are directly readable. `[workspace.sandbox]` wraps every harness and mediated-shell spawn in an isolation backend so the workload cannot read the daemon's sensitive paths or reach its socket. The default is `off`, which preserves single-process behavior unchanged.

The masked set is always derived from the runtime's own path helpers — the config directory (`~/.config/acp-stack`, holding config and the age key) and the state directory (`~/.local/share/acp-stack`, holding the secret store, state database, and local socket) — so an operator cannot misconfigure the protection away. `[workspace.sandbox].mask_paths` only adds to that set.

Backends are selected by `[workspace.sandbox].mode`:

- `off` — no wrapping.
- `unshare` — runs the workload in fresh mount, pid, ipc, and uts namespaces with a private `/proc`, the sensitive paths masked with `tmpfs`, then all capabilities and `no_new_privs` dropped before exec. Requires the daemon to hold `CAP_SYS_ADMIN`, as in a privileged container.
- `bwrap` — the same masking through `bubblewrap`, for hosts with unprivileged user namespaces.
- `custom` — an operator-supplied wrapper argv in `[workspace.sandbox].wrapper`, for any other mechanism such as `systemd-run` or `firejail`.

Secrets referenced in `[agent].env` are still delivered to the harness through its environment under every backend; only on-disk secrets and the control socket are masked. The same wrapping applies to mediated shell commands, so a shell command the agent runs cannot read the daemon's secrets either.

### Network isolation (`unshare` only)

`[workspace.sandbox.network]` adds per-spawn network-namespace isolation to the `unshare` backend. An omitted block means `mode = "host"`: the workload shares the host network stack and the wrapper is unchanged byte for byte. Any non-default network block with a backend other than `unshare` is rejected at config load; in particular `bwrap` network isolation is not implemented and configuring it would imply an unenforced guarantee. `mode = "host"` with provider settings is likewise rejected — a block that looks configured but enforces nothing must not load. Isolated networking also requires working Linux `pidfd_open` and `pidfd_send_signal` syscalls; startup fails closed when the kernel or seccomp policy blocks them. There is no CLI surface for the block — imported or directly edited TOML is the only way to configure it, and `acps workspace sandbox set` refuses (without writing) a mode change that would conflict with a configured network block.

```toml
[workspace.sandbox]
mode = "unshare"

[workspace.sandbox.network]
mode = "isolated"
provider = ["/usr/local/libexec/acps-network-provider", "--config", "/etc/provider.toml"]
provider_timeout = "30s"
provider_stderr = "daemon"
```

`isolated` gives every wrapped spawn (agent harness and each mediated command alike) its own fresh network namespace. With an empty `provider` the namespace is deny-all: acp-stack configures nothing, not even loopback. All network policy — veth devices, routes, DNS, gateways, proxies — belongs to the optional operator-supplied lifecycle provider; acp-stack never injects proxy variables, configures interfaces, resolves DNS, or inspects traffic.

The provider is invoked as `<executable> setup <configured-args...>` when the namespace exists but before the workload runs, and `<executable> teardown <configured-args...>` after the workload exits, each bounded independently by `provider_timeout` (default `30s`). The executable must be an absolute path — mediated spawns can run without `PATH` in their environment, so name resolution is not deterministic. Setup is fail-closed: on nonzero exit or timeout the workload is never executed, teardown is attempted for partial-setup cleanup, and the spawn fails. Both phases must be idempotent. Each phase runs in an internal monitor's process group; a liveness fd makes supervisor death kill the monitor, provider, and descendants, so providers must remain in that inherited group and must not daemonize, call `setsid`, or otherwise detach. The provider runs with the supervisor's privileges, `/` as its working directory (never the agent-writable workload cwd), and a cleared environment holding exactly the contract variables (it must set its own `PATH`); agent environment variables and secrets never reach it:

- `ACPS_SANDBOX_NETWORK_PROTOCOL=1` — provider protocol version.
- `ACPS_SANDBOX_NETWORK_ID=<random-id>` — unique identifier for this wrapped spawn.
- `ACPS_SANDBOX_NETWORK_NAMESPACE=<proc-fd-path>` — namespace handle usable with `setns` or `nsenter`, valid through teardown.
- `ACPS_SANDBOX_NETWORK_PID=<host-pid>` — namespace-owning process PID, guaranteed during setup only (it is omitted at teardown).

Provider stdout is always discarded so it cannot corrupt the ACP transport. Provider stderr goes to the daemon's stderr diagnostic channel (`provider_stderr = "daemon"`, the default) or is discarded (`"null"`); it is never attached to a mediated command's captured output. A provider killed by supervisor SIGKILL must reconcile any host resources not tied to namespace destruction on its next run.

Scope of the guarantee: a network namespace isolates the IPv4/IPv6 stacks and abstract-namespace Unix sockets, but it does not block pathname Unix sockets — those are filesystem objects. The daemon's control socket stays protected by the tmpfs path masking above, not by the network namespace.

```mermaid
flowchart TB
    subgraph Daemon["Daemon — trusted, holds privilege"]
        Sensitive["age key · secret store · config · control socket"]
        Wrap["sandbox::wrap per spawn"]
    end
    subgraph Workload["Workload — untrusted"]
        Harness["agent harness / mediated shell"]
    end
    Wrap -->|"new namespaces · tmpfs-mask sensitive paths · drop caps · no_new_privs"| Harness
    Harness -. cannot read .-> Sensitive
```

## Security Self-Check

The admin-tier public `GET /v1/security/check` route and keyless local `acps security check` diagnostic report findings for common misconfiguration: unsafe binds, wildcard browser origins, excessive auth failures, loose file modes, ownership mismatches, unwritable workspaces, unavailable required dependencies, and external logging delivery failures.

Findings include severity (`warning` or `critical`), code, message, an optional structured `details` payload for findings with machine-readable context, and remediation when an operator action is available.

### History

Every self-check invocation through `GET /v1/security/check` is persisted into the `security_runs` and `security_findings` tables in the local state database. The check response includes the generated `run_id` so operators can correlate the live response with the durable row. Runs are kept indefinitely; pruning is left to future operations work.

- `GET /v1/security/history?limit=N&after=<run-id>` (admin tier) returns recent runs newest-first with aggregate counts and a `next_cursor` for keyset pagination. `limit` defaults to 20 and is capped at 500.
- `GET /v1/security/history/{run_id}` (admin tier) returns a single run with its findings in emit order, replaying exactly what `acps security check` produced.
- `acps security history [--limit N] [--after <id>] [--json]` prints the operator table or raw JSON.
- `acps security show <run-id> [--json]` prints the run summary plus its findings.

Aggregate run status is `succeeded` when no critical findings were emitted and `failed` otherwise; the orthogonal `ok` boolean is true only when neither warnings nor critical findings were emitted.

### Finding categories and remediation coverage

Every emitted finding carries a non-empty remediation. The category-to-code map for the operator-facing self-check is:

- key: `auth.failure_threshold`
- file permission: `runtime.path_ownership`, `runtime.path_mode_loose`, `runtime.path_uninspectable`, `runtime.workspace_not_writable`
- origin and CORS: `http.wildcard_origin_public_bind`, `edge.cloudflare.unsafe_origins`
- proxy: `http.trust_proxy_without_trusted_proxies`, `edge.cloudflare.missing_local_trusted_proxies`
- sink: `logging.supabase.delivery_failing`
- deps: `deps.required_unavailable`
- runtime user: `runtime.user_mismatch`
- bind: `api.public_bind`, `edge.cloudflare.public_bind_tunnel`, `edge.cloudflare.cloudflared_missing`, `edge.cloudflare.headers_missing`, `edge.cloudflare.direct_public_requests`

`deps.required_unavailable` is emitted when required dependency declarations are unavailable. Details include a bounded list of dependency names, kinds, features, and reasons; the complete report remains available from `acps deps check`.
