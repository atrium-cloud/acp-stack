# Config

`acp-stack` reads TOML config from `~/.config/acp-stack/acp-stack.toml` by default. Config files are portable: secret values are stored separately and referenced by name.

## Example

```toml
config_version = 1

[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"
max_request_bytes = 104857600

[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

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
| `[auth]`           | secret reference names for session and admin API keys                |
| `[security.http]`  | origin checks, rate limits, proxy trust, and auth-failure blocking   |
| `[workspace]`      | workspace root, uploads path, shell, runtime user, and file limits   |
| `[agent]`          | configured ACP agent process and injected secret refs                |
| `[agent.provider]` | selected provider/model metadata for provider-backed agents          |
| `[permissions]`    | command and ACP permission policy                                    |
| `[commands]`       | mediated shell command limits and env allowlist                      |
| `[dependencies]`   | expected external programs, runtimes, packages, and MCP declarations |
| `[[mcp.servers]]`  | MCP servers attached to ACP sessions                                 |
| `[edge.cloudflare]` | Cloudflare Tunnel edge profile and managed provisioning refs         |
| `[logging]`        | local logging and optional external sink settings                    |

## API And Security

`[api].bind` is the daemon listener. Use loopback for host deployments and place a proxy or tunnel in front for public access. `[api].public_url` is the external base URL used by clients and CLI calls when set.

`[security.http].allowed_origins` is the browser origin allowlist. Empty means no browser origins are allowed. `trust_proxy_headers = true` accepts forwarded client metadata only from exact IPs listed in `trusted_proxies`.

Both `[api].max_request_bytes` and `[security.http].max_request_bytes` can cap HTTP request bodies. When both are present, the tighter limit is enforced.

## Auth And Secrets

The `[auth]` fields name secret-store entries, not plaintext keys. `acps init` generates the session and admin keys on first run and stores them under these refs. Config import/export never includes secret values.

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

Provider and model fields are documented in [agents/config.md](agents/config.md).

## Permissions And Commands

`[permissions].mode` controls command and ACP permission behavior:

| Mode         | Behavior                                                |
| ------------ | ------------------------------------------------------- |
| `auto`       | allow by default; `review` patterns create audit events; composed shell commands require review |
| `supervised` | unmatched risky actions require approval                |
| `locked`     | unmatched commands require approval                     |

`deny` patterns reject immediately. Pending requests expire after `request_timeout` using `timeout_action`.

Command `deny` and `review` patterns are checked against the full submitted command and each simple command segment separated by shell control operators.

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

## Import And Export

Config import validates TOML, rejects unknown fields and invalid enum values, and writes canonical TOML atomically. Config export returns canonical TOML with secret references only.
