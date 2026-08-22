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
max_request_bytes = 104857600
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
env = ["OPENCODE_API_KEY"]
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

[logging]
level = "info"
local_retention_days = 30

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
| `[workspace.sandbox]` | harness/shell isolation backend (see [security.md](security.md#sandbox)) |
| `[agent]`          | configured ACP agent process and injected secret refs (legacy input; canonical config writes `[array]`) |
| `[agent.auto_update]` | periodic managed agent update policy                             |
| `[agent.provider]` | selected provider/model metadata for provider-backed agents          |
| `[agent.providers]` | explicit active mapped-provider ids and target-scoped alias selections |
| `[array]`          | Array mode flag, primary target, and configured agent targets        |
| `[[array.targets]]` | one ACP agent target; canonical home of each agent block under `[array.targets.agent]` |
| `[updates.acp_stack]` | acp-stack self-update policy                                     |
| `[permissions]`    | command and ACP permission policy                                    |
| `[commands]`       | mediated shell command limits and env allowlist                      |
| `[prompts]`        | stale-prompt sweeper thresholds (see [runtime.md](runtime.md))        |
| `[dependencies]`   | expected external programs, runtimes, packages, and MCP declarations |
| `[[mcp.servers]]`  | MCP servers attached to ACP sessions                                 |
| `[[skills.sources]]` | user-declared Agent Skills sources, alongside the embedded catalog  |
| `[edge.cloudflare]` | Cloudflare Tunnel edge profile and managed provisioning refs         |
| `[logging]`        | local logging and optional external sink settings                    |
| `[local]`          | internal Unix socket override and local session-tier access mode      |
| `[extensions.<name>]` | typed extension instances (network-provider, managed-state)       |

## API And Security

`[api].bind` is the daemon listener. Use loopback for host deployments and place a proxy or tunnel in front for public access. `[api].public_url` is the external base URL used by clients and CLI calls when set.

`[security.http].allowed_origins` is the browser origin allowlist. Empty means no browser origins are allowed. `trust_proxy_headers = true` accepts forwarded client metadata only from exact IPs listed in `trusted_proxies`.

Both `[api].max_request_bytes` and `[security.http].max_request_bytes` are required and cap HTTP request bodies; the tighter of the two is enforced.

`[local].socket_path` optionally overrides the internal Unix socket used by keyless local `acps` routes. When omitted, the daemon binds `~/.local/share/acp-stack/acps-local.sock`.

`[local].session_auth` controls local Unix-socket access to session-tier HTTP routes. The default `session-key` keeps those routes unavailable locally unless callers provide `--session-key` or `ACP_STACK_SESSION_KEY` and use the public API. `keyless` lets same-user local `acps` commands use session-tier HTTP routes through the socket without a bearer key. Admin-tier routes are unaffected.

## Auth And Secrets

Auth keys are not config fields and are not stored in `secrets.age`. `acps init` generates the session and admin keys on first run, prints their plaintext values once, and stores only non-recoverable verifier rows in local state. The loader still accepts a legacy `[auth]` table (`session_key_ref`, `admin_key_ref`) for one-time migration off pre-verifier configs; the published JSON Schema describes the post-migration shape only, so a file carrying `[auth]` loads but does not validate against the schema.

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
| `expected_sha256` | optional pinned digest of the installed harness binary, verified after install |
| `restart` | process restart policy: `on-crash` or `never`             |
| `harness_version` | optional pin to a specific GitHub Release tag for install and managed update (see [cli.md](cli.md)) |

Provider and model fields are documented in [agents/config.md](agents/config.md). Root `agent.model` and `[agent.provider].model` are mutually exclusive.

`[agent.provider]` defines the default lane. Without `[agent.providers]`, the implicit active set is that default provider plus any enabled subagent provider; `[agent.providers].active` optionally replaces it, while `[agent.providers.selected_aliases]` chooses backup-key aliases for that target. Multiple active providers are valid only for harnesses that advertise the capability, initially OpenCode and Pi.

`[agent.install]` is the operator escape hatch for a custom (non-registry) agent: `type = "shell"`, a `shell` snippet that installs the harness (and any adapter), and `creates` — the path that must resolve to an executable after the install runs. When present for an `id` the registry does not know, the runtime drives the agent from `[agent]`/`[agent.install]` directly and skips the registry-only support, provider/model auto-config, and managed auto-update (there is no upstream version to resolve, so `acps agent update set` is rejected and the daemon skips it). `acps init --custom-agent-*` writes this block; an adapter-backed custom agent uses the same shape with `command` pointing at the adapter binary. `[agent.adapter]` is runtime-populated from the registry and rejected if written by hand.

`[agent.adapter_override]` launches a registry agent through an operator-designated ACP adapter while the registry harness install and the other registry-derived behavior stay managed; it applies to any registry kind and is mutually exclusive with `[agent.install]`. Fields: `command` (required; also the adapter's install/update identity), `args`, `github`, at least one of `[agent.adapter_override.install.shell|npm|github]` (same shapes as the registry adapter install variants), and `[agent.adapter_override.update]` with `shell_rerun`. npm and github installs join managed adapter auto-update; a shell-only install is skipped by managed update unless `shell_rerun = true`. `[agent] command`/`args` must equal the override's `command`/`args` — validation rejects a divergence, since the managed lanes would otherwise track the adapter while the supervisor launches the bare harness.

`[agent.auto_update]` controls daemon-side managed agent updates. `frequency` accepts hour/day/week units (minimum 1 hour), e.g. `12h`, `1d`, `3d`, or `4w`. Existing configs without this block do not auto-update until the block is added or init writes it for a supported agent. `acps init` writes this block for a managed registry agent — enabled and daily by default, or set explicitly with `--agent-update <on|off>` / `--agent-update-frequency <freq>` (or the interactive prompt), so auto-update can be declined at init rather than only afterward via `acps agent update set --auto-off`. The daemon auto-updater only runs when the agent is stopped and never interrupts a running agent, so a continuously running agent is skipped each cycle; apply updates to a live agent with `acps agent update --restart`.

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

Cloudflare Tunnel config lives under `[edge.cloudflare]`. `mode = "generated"` writes local cloudflared artifacts only. `mode = "managed"` also requires `api_token_ref` and `account_id_ref`; init resolves those secret refs in memory, creates the tunnel, writes `tunnel_id` back to config before later provisioning steps, pushes the remote tunnel config, creates or updates the proxied CNAME, and writes an owner-only tunnel token env artifact. `exposure` accepts only `tunnel`, and `cloudflared_deployment` is `host`, `docker`, or `external` (default `host`); both are validated only when `enabled = true`.

## MCP Servers

MCP servers can be stdio or HTTP:

```toml
[[mcp.servers]]
type = "stdio"
name = "local-tool"
command = "tool-server"
args = ["serve"]
env = ["TOOL_API_KEY", "DATABASE_URL=postgres://user:${DB_PASS}@host/db"]

[[mcp.servers]]
type = "http"
name = "linear"
url = "https://mcp.linear.app/mcp"
headers = [{ name = "Authorization", value_ref = "LINEAR_API_KEY" }]

[[mcp.servers]]
type = "http"
name = "parallel"
url = "https://api.parallel.example/mcp"
headers = [{ name = "Authorization", value = "Bearer ${PARALLEL_API_KEY}" }]
```

Secret refs are resolved at session attach time. Secret values do not appear in config export, API responses, or durable logs. HTTP server URLs must be https, or http toward a loopback host (a local relay never leaves the host). At daemon startup a server declaration that fails these per-server rules is skipped with a startup warning rather than failing the boot — the daemon degrades instead of bricking on one bad declaration; declaration and config-write paths still reject it.

### Secret-Reference Templates

Value positions (HTTP header `value`, stdio `env` entries, and `[agent].env` entries) accept interpolated templates in addition to whole-value refs:

- A header sets exactly one of `value_ref` (whole-value secret ref, as before) or `value` (a template). Setting both or neither fails validation.
- An `env` entry is either a bare ref name `NAME` (env var `NAME` receives the whole secret `NAME`, as before) or `VAR=template` (env var `VAR` receives the composed template value).
- Template syntax: `${NAME}` interpolates the secret `NAME` at resolve time; `$$` is a literal `$`; any other `$` is rejected. A template must contain at least one `${NAME}` reference — pure literals are rejected so plaintext credentials cannot be pasted into config.
- Refs inside templates may repeat freely across the config (composing one secret into several values is the point); whole-value declarations keep the existing duplicate-ref rejection. Within one `env` list, the produced env var names must be unique.
- The looks-like-a-secret screening that applies to ref names also runs over template literals and the refs inside `${}`; the literals are additionally screened concatenated, so a credential split across a `${}` boundary still trips the heuristic.

acp-stack does not interpret the composed value: the referenced secret may be an opaque token minted elsewhere and the URL a local relay endpoint, and nothing behaves differently.

## Skills

`[[skills.sources]]` declares user Agent Skills sources layered alongside the embedded curated catalog. Each entry sets `alias` (unique, lowercase-alphanumeric with single dashes, at most 64 characters), `github` (`owner/repo`; the owner is a GitHub account name — alphanumerics and dashes, at most 39 characters — and the repo allows alphanumerics, `-`, `_`, `.`, at most 100 characters, not `.` or `..`), optional `branch` (default `main`; non-empty, at most 255 characters, no leading or trailing `/`, and a git-ref-safe charset — letters, digits, `-`, `_`, `.`, `/`, no `..` — since it is interpolated into the archive URL), and optional `trusted` (default `false`; an operator assertion recorded and surfaced, not enforced). The alias is then usable anywhere a source is accepted — `acps skills add <alias> <skill>` and `acps skills source get <alias>`. The catalog is resolved before user sources, so `acps skills source add` refuses an alias that matches a curated one and a hand-written collision is inert (the curated source wins). An individually invalid entry does not fail startup: like an invalid MCP server declaration, it is skipped with a warning at daemon startup and quietly on later runtime reloads (re-warning on every reload would spam the log), while the strict write path (`acps skills source add`) still rejects invalid new entries; a `source add`/`remove` write also drops any previously-skipped invalid entries from the file, with a warning naming each dropped alias. Skills are discovered flat under the repo's `skills/` directory, the same convention as `github:<owner>`; nested, frontmatter-indexed layouts are the curated catalog's domain. Entries are managed with `acps skills source add`/`remove`, which edit this table through the daemon; there is no install action until `acps skills add` runs.

```toml
[[skills.sources]]
alias = "my-org"
github = "my-org/skills"
branch = "main"
trusted = false
```

## Extensions

`[extensions.<name>]` declares typed integration seam instances. Each instance carries `type = "network-provider"` (per-spawn network isolation with an external provider executable; unshare backend only, at most one instance) or `type = "managed-state"` with `capability = "provider-credential"` (a state namespace owned by an external orchestrator through the admin apply endpoint). Fields that do not belong to the declared type are rejected. A network-provider instance accepts `provider`, `provider_timeout`, `provider_stderr`, and `workload_env`; a managed-state instance accepts `capability`. `[extensions.<name>.workload_env]` is a string table of environment variables injected into every workload spawned inside the provider's namespace (at most 16 entries, names matching `[A-Za-z_][A-Za-z0-9_]*` and at most 128 bytes, non-empty values at most 16 KiB, with `PATH` and `HOME` rejected); it never reaches the provider executable and is applied after `[agent].env` so it wins on conflict. The former `[workspace.sandbox.network]` block was replaced by the network-provider type and fails config load with a migration error. Contracts and examples are in [extensions.md](extensions.md).

## Import And Export

Config import validates TOML, rejects unknown fields and invalid enum values, and writes canonical TOML atomically. Config export reads the current config file and returns canonical TOML with secret references only.
