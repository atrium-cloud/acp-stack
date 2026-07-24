# Config

`acp-stack` reads TOML config from `~/.config/acp-stack/acps-config.toml` by default. Config files are portable: secret values are stored separately and referenced by name.

## Example

```toml
config_version = 1

[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"
max_request_bytes = 104857600

[security.http]
allowed_origins = ["https://agent.example.com"]
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
trust_proxy_headers = false
trusted_proxies = []

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
restart = "on-crash"

[agent.auto_update]
enabled = true
frequency = "1d"

[updates.acp_stack]
policy = "security-critical"
frequency = "1d"

[permissions]
mode = "auto"
review = ["sudo *", "rm *"]
deny = ["shutdown*", "reboot*"]
request_timeout = "5m"
timeout_action = "deny"

[commands]
default_timeout = "10m"
cancel_grace = "5s"
progress_interval = "30s"
env_allowlist = ["GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL"]
max_output_bytes = 1048576

[[mcp.servers]]
type = "http"
name = "linear"
url = "https://mcp.linear.app/mcp"
headers = [{ name = "Authorization", value_ref = "LINEAR_API_KEY" }]
```

## Top-Level Sections

| Section            | Purpose                                                              |
| ------------------ | -------------------------------------------------------------------- |
| `[api]`            | HTTP bind address, public URL, and request size cap                  |
| `[security.http]`  | origin checks, rate limits, proxy trust, and auth-failure blocking   |
| `[workspace]`      | workspace root, uploads path, shell, runtime user, and file limits   |
| `[agent]`          | configured ACP agent process and injected secret refs (legacy input; canonical config writes `[array]`) |
| `[agent.auto_update]` | periodic managed agent update policy                             |
| `[agent.provider]` | selected provider/model metadata for provider-backed agents          |
| `[agent.providers]` | explicit active mapped-provider ids and target-scoped alias selections |
| `[array]`          | Array mode flag, primary target, and configured agent targets        |
| `[[array.targets]]` | one ACP agent target; canonical home of each agent block under `[array.targets.agent]` |
| `[updates.acp_stack]` | acp-stack self-update policy                                     |
| `[permissions]`    | command and ACP permission policy                                    |
| `[commands]`       | mediated shell command limits and env allowlist                      |
| `[dependencies]`   | expected external programs, runtimes, packages, and MCP declarations |
| `[[mcp.servers]]`  | MCP servers attached to ACP sessions                                 |
| `[edge.cloudflare]` | Cloudflare Tunnel edge profile and managed provisioning refs         |
| `[logging]`        | local logging and optional external sink settings                    |
| `[local]`          | internal Unix socket override and local session-tier access mode      |
| `[extensions.<name>]` | typed extension instances (network-provider, managed-state)       |

## API And Security

`[api].bind` is the daemon listener. Use loopback for host deployments and place a proxy or tunnel in front for public access. `[api].public_url` is the external base URL used by clients and CLI calls when set.

`[security.http].allowed_origins` is the browser origin allowlist. Empty means no browser origins are allowed. `trust_proxy_headers = true` accepts forwarded client metadata only from exact IPs listed in `trusted_proxies`.

Both `[api].max_request_bytes` and `[security.http].max_request_bytes` can cap HTTP request bodies. When both are present, the tighter limit is enforced.

`[local].socket_path` optionally overrides the internal Unix socket used by keyless local `acps` routes. When omitted, the daemon binds `~/.local/share/acp-stack/acps-local.sock`.

`[local].session_auth` controls local Unix-socket access to session-tier HTTP routes. The default `session-key` keeps those routes unavailable locally unless callers provide `--session-key` or `ACP_STACK_SESSION_KEY` and use the public API. `keyless` lets same-user local `acps` commands use session-tier HTTP routes through the socket without a bearer key. Admin-tier routes are unaffected.

## Auth And Secrets

Auth keys are not config fields and are not stored in `secrets.age`. `acps init` generates the session and admin keys on first run, prints their plaintext values once, and stores only non-recoverable verifier rows in local state.

Fields that expect secret refs reject likely pasted secret values. Use `acps secrets set <name>` to store the value, then reference `<name>` in config.

## Workspace

`[workspace].root` and `[workspace].uploads` must be absolute paths. Workspace API paths are always resolved under `root`; traversal outside the root is rejected.

`max_file_bytes` caps file reads, writes, uploads, and downloads. It is separate from the HTTP request body cap because workspace reads and downloads may not have an inbound request body.

Workspace sources can be declared for first-run materialization:

```toml
[[workspace.code_sources]]
type = "git"
repo = "https://github.com/example/project.git"
branch = "main"
credential_ref = "GITHUB_TOKEN"

[[workspace.data_sources]]
type = "https"
url = "https://example.com/dataset.tar.gz"
expected_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

Supported code sources: Git repositories. Supported data sources: absolute local paths, HTTPS downloads, and S3 objects. Downloads and extraction are size-capped; archives cannot write outside their destination.

## Agent

`[agent]` describes the process `acp-stack` launches:

| Field     | Meaning                                                   |
| --------- | --------------------------------------------------------- |
| `id`      | embedded agent catalog id                                 |
| `name`    | display name                                              |
| `command` | executable                                                |
| `args`    | argv after the executable                                 |
| `cwd`     | launch directory; defaults to workspace root when omitted |
| `env`     | secret refs injected as environment variables             |
| `restart` | process restart policy: `on-crash` or `never`             |

Provider and model fields are documented in [agents/config.md](agents/config.md). Root `agent.model` and `[agent.provider].model` are mutually exclusive.

`[agent.provider]` defines the default lane. Without `[agent.providers]`, the implicit active set is that default provider plus any enabled subagent provider; `[agent.providers].active` optionally replaces it, while `[agent.providers.selected_aliases]` chooses backup-key aliases for that target. Multiple active providers are valid only for harnesses that advertise the capability, initially OpenCode and Pi.

`[agent.install]` is the operator escape hatch for a custom (non-registry) agent: `type = "shell"`, a `shell` snippet that installs the harness (and any adapter), and `creates` — the path that must resolve to an executable after the install runs. When present for an `id` the registry does not know, the runtime drives the agent from `[agent]`/`[agent.install]` directly and skips the registry-only support and provider/model auto-config. `acps init --custom-agent-*` writes this block; an adapter-backed custom agent uses the same shape with `command` pointing at the adapter binary. `[agent.adapter]` is runtime-populated from the registry and rejected if written by hand.

`[agent.auto_update]` controls daemon-side managed agent updates. `frequency` uses duration suffixes such as `12h`, `1d`, `3d`, or `4w`. Existing configs without this block do not auto-update until the block is added or init writes it for a supported agent. The daemon auto-updater only runs when the agent is stopped and never interrupts a running agent, so a continuously running agent is skipped each cycle; apply updates to a live agent with `acps agent update --restart`.

`[updates.acp_stack]` controls updates of `acp-stack` itself from GitHub Releases. `policy = "security-critical"` is the default and auto-installs only same-major, non-breaking releases marked security-critical. `compatible` also permits same-major, non-breaking regular releases. `manual` disables auto-install. `frequency` uses day/week granularity (minimum a day). `acps init` writes this block — `--stack-update <on|security|off>` and `--stack-update-frequency <freq>`, or the interactive auto-update prompt. Docker and Railway deployments are check-only and should be updated by redeploying the image.

## Array

`[array]` runs more than one agent target under one runtime, with `primary_target` backing the default `[agent]` and `/v1/agent/*` surfaces. In canonical config each agent block lives under `[array.targets.agent]`; a top-level `[agent]` block is accepted as legacy input and migrated into a single Array target with `enabled = false`. Array is disabled by default, so a single-agent config behaves exactly as before. See [array.md](array.md) for the full model, validation rules, CLI, and API.

## Permissions And Commands

`[permissions].mode` controls command and ACP permission behavior:

| Mode         | Behavior                                                |
| ------------ | ------------------------------------------------------- |
| `auto`       | allow by default; `review` patterns create audit events; composed shell commands require review |
| `supervised` | unmatched risky actions require approval                |
| `locked`     | unmatched commands require approval                     |

`deny` patterns reject immediately. Pending requests expire after `request_timeout` using `timeout_action`.

Command `deny` and `review` patterns are checked against raw and shell-word-normalized forms of the full submitted command and each simple command segment found through shell control operators, command substitution, or process substitution. Shell word construction in the command word requires review when no policy pattern matches.

`[commands].env_allowlist` is the only non-secret environment forwarded into mediated shell commands. Secret refs are injected only through explicit agent or MCP configuration.

## Logging

`[logging.supabase]` mirrors selected local state rows to Supabase when enabled. New table-backed setups should use `acps logging supabase setup --url ...`, which provisions prefixed `public` tables through the Supabase CLI and stores a narrow writer DB URL under `db_url_ref`. The legacy `postgrest` backend uses `api_key_ref` for a Supabase secret key and requires pre-provisioned/exposed tables. `acps logging supabase check` writes a marked canary row to verify the configured backend.

## Dependencies

`[dependencies]` declares expected tools and optional operator-provided install actions:

```toml
[[dependencies.commands]]
name = "cloudflared"
required = true
feature = "cloudflare-tunnel"

[dependencies.commands.install]
shell = "curl -fsSL https://pkg.cloudflare.com/install.sh | sh"
creates = "cloudflared"
scope = "user"
timeout_secs = 600
```

Only `commands` entries may declare install actions. `packages`, `runtimes`, and `mcp` entries are declarative checks. Runtime entries are executable checks; package entries use local Linux package databases when available.

## Edge

Cloudflare Tunnel config lives under `[edge.cloudflare]`. `mode = "generated"` writes local cloudflared artifacts only. `mode = "managed"` also requires `api_token_ref` and `account_id_ref`; init resolves those secret refs in memory, creates the tunnel, writes `tunnel_id` back to config before later provisioning steps, pushes the remote tunnel config, creates or updates the proxied CNAME, and writes an owner-only tunnel token env artifact.

## MCP Servers

MCP servers can be stdio or HTTP:

```toml
[[mcp.servers]]
type = "stdio"
name = "local-tool"
command = "tool-server"
args = ["serve"]
env = ["TOOL_API_KEY"]

[[mcp.servers]]
type = "http"
name = "linear"
url = "https://mcp.linear.app/mcp"
headers = [{ name = "Authorization", value_ref = "LINEAR_API_KEY" }]
```

Secret refs are resolved at session attach time. Secret values do not appear in config export, API responses, or durable logs.

## Extensions

`[extensions.<name>]` declares typed integration seam instances. Each instance carries `type = "network-provider"` (per-spawn network isolation with an external provider executable; unshare backend only, at most one instance) or `type = "managed-state"` with `capability = "provider-credential"` (a state namespace owned by an external orchestrator through the admin apply endpoint). Fields that do not belong to the declared type are rejected. The former `[workspace.sandbox.network]` block was replaced by the network-provider type and fails config load with a migration error. Contracts and examples are in [extensions.md](extensions.md).

## Import And Export

Config import validates TOML, rejects unknown fields and invalid enum values, and writes canonical TOML atomically. Config export reads the current config file and returns canonical TOML with secret references only.
