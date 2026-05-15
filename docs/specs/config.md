# Config

The config is the portable environment definition. It is stored as TOML and is safe to share by default because it contains secret references, not secret values.

## Files

Default config path:

```text
~/.config/acp-stack/acp-stack.toml
```

Default instance-local paths:

```text
~/.local/share/acp-stack/state.sqlite
~/.local/share/acp-stack/secrets.age
~/.config/acp-stack/age.key
```

The config describes desired runtime state. SQLite records runtime history. The age-backed secret store contains secret values.

## Example

```toml
[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"
max_request_bytes = 104857600

[auth]
session_key_ref = "ACP_STACK_SESSION_KEY"
admin_key_ref = "ACP_STACK_ADMIN_KEY"

[security.http]
max_request_bytes = 104857600
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = ["https://agent.example.com"]
trust_proxy_headers = false
trusted_proxies = []

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[workspace.source]
type = "git" # none | git | s3
repo = "https://github.com/example/project.git"
branch = "main"
dest = "/workspace/project"
credential_ref = "GITHUB_TOKEN"

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
api_key_ref = "SUPABASE_SECRET_KEY"
schema = "acp_stack"

[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["--acp"]
cwd = "/workspace"
env = ["OPENCODE_API_KEY"]
# expected_sha256 is optional; when present it must be exactly 64 lowercase hex chars.
restart = "on-crash"

[agent.install]
type = "registry"
id = "opencode"
creates = "opencode"

[permissions]
mode = "auto"                # auto | supervised | locked
review = ["sudo *", "rm *"]
deny = ["shutdown*", "reboot*"]
request_timeout = "5m"       # how long a pending row sits before the timer fires
timeout_action = "deny"      # deny | approve — what the timer does on expiry

[commands]
default_timeout = "10m"      # 5s, 750ms, 1h all accepted
cancel_grace = "5s"
env_allowlist = ["GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL"]
max_output_bytes = 1048576

[dependencies]
# Each entry: { name, required = true, feature = "<optional label>" }
commands = [{ name = "git" }, { name = "ripgrep", required = false }]
packages = []
runtimes = []
mcp = [{ name = "slack" }]    # cross-references [[mcp.servers]] names

[[mcp.servers]]
type = "stdio"
name = "slack"
command = "slack-mcp"
args = []
env = ["SLACK_BOT_TOKEN", "SLACK_TEAM_ID"]

[[mcp.servers]]
type = "http"
name = "linear"
url = "https://api.linear.app/mcp"
headers = [{ name = "Authorization", value_ref = "LINEAR_API_KEY" }]
```

`[permissions]` controls how `POST /v1/commands` evaluates each submitted shell string AND how ACP `session/request_permission` requests are gated. Patterns on `deny` reject the submission immediately with `command.denied`. Patterns on `review` and unmatched submissions in `locked` mode create a pending row in `permission_requests`, publish a `permission.created` event on the `permissions` WebSocket topic, and block the request until an operator decides via `/v1/permissions/{id}/approve` or `/v1/permissions/{id}/deny`. In `auto` mode, `review` matches still proceed and emit a `command.review_flagged` event for audit.

`request_timeout` is the per-row duration before the timer fires; `timeout_action` chooses how the timer settles a still-pending row (`deny` writes `expired`; `approve` auto-approves with no `option_id`). Defaults are `5m` / `deny`.

`[commands]` controls the Command Gateway runtime. `default_timeout` and `cancel_grace` accept short duration suffixes (`ms`, `s`, `m`, `h`). `env_allowlist` is the closed set of environment variable names the daemon will forward into command children; secrets from the encrypted store are never injected implicitly. `max_output_bytes` caps the total bytes persisted per run; once exceeded the row's `truncated` flag is set and further output is drained but not stored.

`[dependencies]` declares external programs, packages, runtimes, and MCP servers that the operator expects to be available. The runtime reports their satisfaction status via `GET /v1/deps` and `acps deps check` but does not install anything. Today only `commands` are checked (PATH lookup); `packages` and `runtimes` are declarative-only with `<kind>-check-not-implemented` reasons; `mcp` cross-references `[[mcp.servers]]` for declaration presence.

`[mcp.servers]` declares MCP servers passed to the agent at session create/load/resume time. Each entry is either `type = "stdio"` (with `command`, optional `args`, and an `env` list of secret-ref names) or `type = "http"` (with `url` and a `headers` list of `{ name, value_ref }`). Stdio env values and HTTP header values are resolved from the encrypted secret store on every session call — they never enter the durable event log or any HTTP response. `mcp.session_attached` events record only the server names attached to a session.

`[security.http].trusted_proxies` is a list of exact IP-address strings (no CIDR) trusted to populate `X-Forwarded-For` / `Forwarded` headers. When `trust_proxy_headers = true` and the socket peer matches an entry, the leftmost forwarded IP is used as the client IP for auth-failure tracking. With `trust_proxy_headers = false` or an empty list, the socket peer is always used.

`[agent]` names the ACP process that `acp-stack` launches. For a native ACP agent such as OpenCode, omit `[agent.adapter]`. OpenCode remains a good direct-key example because it uses API keys rather than browser OAuth.

For an adapter-backed agent, `[agent.adapter]` records the registry adapter executable and the upstream agent it wraps; `agent.adapter.id` should match the ACP registry entry when the adapter is distributed through `agentclientprotocol/registry`. As of 2026-05-15, the externally identified adapter-backed agents are Claude Agent, Codex CLI, and Pi. Treat that list as ecosystem data resolved from the registry/Zed ACP pages rather than a baked-in allowlist.

Example adapter-backed Codex config for API-key deployments:

```toml
[agent]
id = "codex"
name = "Codex"
command = "codex-acp"
args = []
cwd = "/workspace"
env = ["OPENAI_API_KEY"]
restart = "on-crash"

[agent.adapter]
id = "codex-acp"
name = "Codex ACP Adapter"
upstream_agent = "codex-cli"
source_url = "https://github.com/zed-industries/codex-acp"

[agent.install]
type = "registry"
id = "codex-acp"
creates = "codex-acp"
```

Do not pass browser OAuth sessions or account cookies through `acp-stack` config or secrets. Codex can be used with OAuth in other environments, but the initial `acp-stack` runtime supports headless direct-key operation only, so the Codex adapter example uses `OPENAI_API_KEY`.

The operator-facing installation flow resolves agents from the ACP registry, not arbitrary third-party install scripts. Direct `[agent.install] type = "shell"` recipes are a low-level/manual escape hatch only. Registry installs fetch `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json` by default, select the configured `agent.install.id`, and use supported registry package distributions (`npx` through `npm install -g`, or `uvx` through `uv tool install`) to make `agent.install.creates` available.

## Import And Export

Config import validates TOML before applying it. Config export returns secret references only.

Related commands are defined in [cli](cli.md):

- `acps config validate [path]`
- `acps config export [--output path]`
- `acps config export --base64`
- `acps config import <path>`
- `acps config import --base64 <code>`

The 0.0.1 implementation supports validation, export, and import:

- `acps config validate [path]` loads an explicit path or `~/.config/acp-stack/acp-stack.toml`.
- `acps config export [--output path]` loads the default config and emits canonical TOML.
- `acps config export --base64` emits the same canonical TOML as base64.
- `acps config import <path>` parses, validates, and atomically writes canonical TOML to `~/.config/acp-stack/acp-stack.toml`. Without `--force`, it refuses to overwrite an existing config. `--base64 <code>` decodes its argument as base64-encoded canonical TOML before validation. Atomic writes use a temp-file + rename under owner-only mode (`0600`).
- Validation rejects unknown fields, invalid enum values, relative workspace paths, missing `workspace.source`, incomplete `git` or `s3` source declarations, fields that do not belong to the selected source type, and aliased or empty `[auth].session_key_ref` / `[auth].admin_key_ref`.

## Request Size Limits

Both `[api].max_request_bytes` and `[security.http].max_request_bytes` cap inbound HTTP request bodies. They are independent fields so that an operator can tighten security limits without changing the headline `[api]` cap, or vice versa. When both are present, the runtime enforces the tighter of the two — `min([api].max_request_bytes, [security.http].max_request_bytes)`. Oversized requests are rejected with 413 before any route handler runs.

`[workspace].max_file_bytes` is a separate, per-file ceiling that the workspace API applies to reads, writes, uploads, and downloads. It is independent of the HTTP body cap because workspace operations also include reads and downloads (which the HTTP body cap does not see) and because the natural file-size limit can be lower than the body cap. Files larger than this limit cannot be transferred through the workspace API regardless of how the bytes are framed on the wire.

## Hardening

Config import/export hardening belongs to the 0.0.4 line:

- validate imported config paths are absolute where required
- reject imported config with secret values in fields that must be references
- support import dry-run output
- check export redaction
- include a config compatibility version field
- test malformed, oversized, and unsafe imports
