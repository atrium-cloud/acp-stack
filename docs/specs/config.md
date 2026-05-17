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
args = ["acp"]
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
# expected_sha256 is optional; when present it must be exactly 64 lowercase hex chars.
restart = "on-crash"

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
mcp = [{ name = "linear" }]    # cross-references [[mcp.servers]] names

[[mcp.servers]]
type = "http"
name = "linear"
url = "https://mcp.linear.app/mcp"
headers = [{ name = "Authorization", value_ref = "LINEAR_API_KEY" }]
```

`[permissions]` controls how `POST /v1/commands` evaluates each submitted shell string AND how ACP `session/request_permission` requests are gated. Patterns on `deny` reject the submission immediately with `command.denied`. Patterns on `review` and unmatched submissions in `locked` mode create a pending row in `permission_requests`, publish a `permission.created` event on the `permissions` WebSocket topic, and block the request until an operator decides via `/v1/permissions/{id}/approve` or `/v1/permissions/{id}/deny`. In `auto` mode, `review` matches still proceed and emit a `command.review_flagged` event for audit.

`request_timeout` is the per-row duration before the timer fires; `timeout_action` chooses how the timer settles a still-pending row (`deny` writes `expired`; `approve` auto-approves with no `option_id`). Defaults are `5m` / `deny`.

`[commands]` controls the Command Gateway runtime. `default_timeout` and `cancel_grace` accept short duration suffixes (`ms`, `s`, `m`, `h`). `env_allowlist` is the closed set of environment variable names the daemon will forward into command children; secrets from the encrypted store are never injected implicitly. `max_output_bytes` caps the total bytes persisted per run; once exceeded the row's `truncated` flag is set and further output is drained but not stored.

`[dependencies]` declares external programs, packages, runtimes, and MCP servers that the operator expects to be available. The runtime reports their satisfaction status via `GET /v1/deps` and `acps deps check` but does not install anything. Today only `commands` are checked (PATH lookup); `packages` and `runtimes` are declarative-only with `<kind>-check-not-implemented` reasons; `mcp` cross-references `[[mcp.servers]]` for declaration presence.

`[mcp.servers]` declares MCP servers passed to the agent at session create/load/resume time. Each entry is either `type = "stdio"` (with `command`, optional `args`, and an `env` list of secret-ref names) or `type = "http"` (with `url` and a `headers` list of `{ name, value_ref }`). Stdio env values and HTTP header values are resolved from the encrypted secret store on every session call — they never enter the durable event log or any HTTP response. `mcp.session_attached` events record only the server names attached to a session. See [mcp.md](mcp.md) for the Linear HTTP MCP example with secret setup.

`[security.http].trusted_proxies` is a list of exact IP-address strings (no CIDR) trusted to populate `X-Forwarded-For` / `Forwarded` headers. When `trust_proxy_headers = true` and the socket peer matches an entry, the leftmost forwarded IP is used as the client IP for auth-failure tracking. With `trust_proxy_headers = false` or an empty list, the socket peer is always used.

`[agent]` names the ACP process that `acp-stack` launches. `[agent].id` matches an entry in the embedded `data/agents.toml`; the runtime uses that lookup to decide whether the agent is native or adapter-backed and what install plan to run. Operators do not write `[agent.adapter]` — that block is populated at runtime from the resolved registry entry and is rejected with an unknown-field error if it appears in operator TOML.

`[agent].harness_version` (optional) pins the harness install to a specific GitHub Release tag when the resolved registry harness uses `github_release`. Omit for latest/floating shell bootstrap entries.

Example native OpenCode config with explicit provider setup. Install metadata flows from the embedded registry; operator TOML stays terse:

```toml
[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
restart = "on-crash"

[agent.provider]
id = "<provider-id>"
model = "<provider-id>/<model-id>"
api_key_ref = "<provider-api-key-ref>"
```

`[agent].model` is optional and used for model-only agents such as Cursor CLI. `[agent.provider]` is optional and used for agents whose provider setup is explicit. `id` is the configured provider id, `model` is the optional agent-specific model id, and `api_key_ref` is the secret ref that should be present in `[agent].env` and referenced from generated agent-owned config. `acps init --provider <provider-id>` can create the initial provider block without a model. `acps agent set --provider <provider-id> [--model <model>] [--api-key-ref <ref>]` edits this block only for registry entries that declare `set_provider = true`. `acps agent set --model <model>` writes `[agent].model` only for model-only entries that declare `set_model = true` and `set_provider = false`. Both paths validate model values against ACP session config options, regenerate supported agent config before writing the main config, and write `acp-stack.toml` only after generated config provisioning succeeds. If provider-backed `--model` is omitted, interactive terminals prompt from the ACP `model` config option; non-interactive runs list advertised model values and exit without mutating config.

Do not pass browser OAuth sessions or account cookies through `acp-stack` config or secrets.

For an agent that is not in the embedded registry (private fork, unreleased build), declare `[agent.install] type = "shell"` as an escape hatch with a free-form install script and a `creates` postcheck. The registry-driven install path is implicit when `[agent.install]` is omitted.

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

The Phase 4 init flow will replace the single `[workspace.source]` seed model with separate code and data source declarations. The planned shape is:

- `[[workspace.code_sources]]` for Git repositories cloned into
  `/workspace/usr/code/<repo-name>/`
- `[[workspace.data_sources]]` for local paths, public HTTPS archives, and S3
  bucket/prefix inputs placed under `/workspace/usr/data/<data-dir-name>/`

Repos use their repository name. Data sources use the existing directory name, archive name, single top-level archive directory, or S3 bucket/prefix terminal name. Init refuses to merge into non-empty destinations unless a later explicit overwrite/force contract is added.

Init does not infer model config from API-key refs. It may write provider/auth config after provider selection and required ref collection. `acps agent set` writes supported model config after the model is explicit. `acp-stack` does not store provider API key values in plaintext.

Phase 4 expands this into a unified provider/model API that resolves provider ids through `data/mapping.toml`, validates selected model and mode values against ACP session config options, updates the agent-owned config file, and relaunches the active agent.

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
