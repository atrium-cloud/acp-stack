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
config_version = 1

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

[[workspace.code_sources]]
type = "git"
repo = "https://github.com/example/project.git"
branch = "main"
credential_ref = "GITHUB_TOKEN"
# name = "project" # optional override; defaults to the repository leaf name

[[workspace.data_sources]]
type = "https"
url = "https://example.com/dataset.tar.gz"
expected_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
# max_download_bytes / max_extracted_bytes are optional safety caps.

[logging]
level = "info"
local_retention_days = 30

[logging.supabase]
enabled = false
url = "https://example.supabase.co"
service_role_key_ref = "SUPABASE_SERVICE_ROLE_KEY"
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
# Each entry: { name, required = true, feature = "<optional label>",
#               install = <optional install action, commands only> }
packages = []
runtimes = []
mcp = [{ name = "linear" }]    # cross-references [[mcp.servers]] names

# Use array-of-tables syntax for `commands` so each entry can carry
# the optional `[install]` block. Mixing inline-array `commands = [..]`
# with `[[dependencies.commands]]` later in the same file is invalid
# TOML.
[[dependencies.commands]]
name = "git"

[[dependencies.commands]]
name = "ripgrep"
required = false

# Optional per-command install action consumed by `acps deps apply`.
# `install` is rejected on `packages`, `runtimes`, and `mcp` — only
# `commands` is actionable. The runner runs `shell` through
# `[workspace].default_shell -c`, captures stdout/stderr/exit, then
# verifies `creates` (PATH name or absolute path) resolves to an
# executable file. `scope = "system"` declares the action needs root;
# the daemon refuses to run it under a non-root uid.
[[dependencies.commands]]
name = "cloudflared"
required = true
feature = "cloudflare-tunnel"

[dependencies.commands.install]
shell = "curl -fsSL https://pkg.cloudflare.com/install.sh | sh"
creates = "cloudflared"
scope = "user"          # "user" (default) or "system"
# timeout_secs = 600    # optional override; default 10m

[[mcp.servers]]
type = "http"
name = "linear"
url = "https://mcp.linear.app/mcp"
headers = [{ name = "Authorization", value_ref = "LINEAR_API_KEY" }]
```

`[permissions]` controls how `POST /v1/commands` evaluates each submitted shell string AND how ACP `session/request_permission` requests are gated. Patterns on `deny` reject the submission immediately with `command.denied`. Patterns on `review` and unmatched submissions in `locked` mode create a pending row in `permission_requests`, publish a `permission.created` event on the `permissions` WebSocket topic, and block the request until an operator decides via `/v1/permissions/{id}/approve` or `/v1/permissions/{id}/deny`. In `auto` mode, `review` matches still proceed and emit a `command.review_flagged` event for audit.

`request_timeout` is the per-row duration before the timer fires; `timeout_action` chooses how the timer settles a still-pending row (`deny` writes `expired`; `approve` auto-approves with no `option_id`). Defaults are `5m` / `deny`.

`[commands]` controls the Command Gateway runtime. `default_timeout` and `cancel_grace` accept short duration suffixes (`ms`, `s`, `m`, `h`). `env_allowlist` is the closed set of environment variable names the daemon will forward into command children; secrets from the encrypted store are never injected implicitly. `max_output_bytes` caps the total bytes persisted per run; once exceeded the row's `truncated` flag is set and further output is drained but not stored.

`[dependencies]` declares external programs, packages, runtimes, and MCP servers that the operator expects to be available. The runtime reports their satisfaction status via `GET /v1/deps` and `acps deps check` and runs declared install actions via `acps deps apply` / `POST /v1/deps/apply`. Only `commands` may declare an `[install]` block (validation rejects `install` on `packages`/`runtimes`/`mcp`); `apply` runs each declared snippet after explicit confirmation, captures stdout/stderr/exit/timestamps into `installer_runs` tagged `agent_id = "deps_apply"` / `step = "deps_apply"`, verifies the `creates` postcheck resolves to an executable file, and prints before/after status. The runtime never derives an apt/brew/yum invocation — every actionable install is operator-declared verbatim. `packages` and `runtimes` are declarative-only with `<kind>-check-not-implemented` reasons; `mcp` cross-references `[[mcp.servers]]` for declaration presence.

`[mcp.servers]` declares MCP servers passed to the agent at session create/load/resume time. Each entry is either `type = "stdio"` (with `command`, optional `args`, and an `env` list of secret-ref names) or `type = "http"` (with `url` and a `headers` list of `{ name, value_ref }`). Stdio env values and HTTP header values are resolved from the encrypted secret store on every session call — they never enter the durable event log or any HTTP response. `mcp.session_attached` events record only the server names attached to a session. See [mcp.md](mcp.md) for the Linear HTTP MCP example with secret setup.

`[security.http].trusted_proxies` is a list of exact IP-address strings (no CIDR) trusted to populate `X-Forwarded-For` / `Forwarded` headers. When `trust_proxy_headers = true` and the socket peer matches an entry, the leftmost forwarded IP is used as the client IP for auth-failure tracking. With `trust_proxy_headers = false` or an empty list, the socket peer is always used.

## Edge Config

`[edge.cloudflare]` configures deployments that expose `acps` through Cloudflare. The preferred mode is Cloudflare Tunnel: keep `[api].bind` on loopback, run `cloudflared` locally, and publish the configured hostname from Cloudflare's edge. The runtime does not bundle Cloudflare credentials or `cloudflared`.

Planned shape:

```toml
[edge.cloudflare]
enabled = true
mode = "generated"          # managed is reserved and rejected for now
exposure = "tunnel"
hostname = "agent.example.com"
tunnel_name = "acp-stack"
cloudflared_deployment = "host" # host | docker | external
```

Generated mode creates local `cloudflared` artifacts only: an ingress config mapping `https://agent.example.com` to `http://127.0.0.1:7700`, a systemd unit snippet, a Docker Compose snippet, and a checklist for creating the tunnel/public hostname in Cloudflare. Managed mode is deferred; configs using `mode = "managed"` fail validation with a not-implemented error.

The recommended hardened local config for tunnel deployments is:

```toml
[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"

[security.http]
allowed_origins = ["https://agent.example.com"]
trust_proxy_headers = true
trusted_proxies = ["127.0.0.1", "::1"]
```

When trusted proxy validation succeeds, observability accepts bounded Cloudflare metadata such as `CF-Connecting-IP`, `CF-IPCountry`, `CF-Ray`, and optional visitor-location headers. Direct requests that bypass Cloudflare remain visible as direct-origin traffic in security logs and self-check findings.

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

[agent.provider.custom]
name = "<provider-display-name>"
base_url = "https://api.example.com/v1"
api = "chat-completions"
model_name = "<model-display-name>"
context = 200000
output_max_tokens = 65536

[agent.subagent.provider]
id = "<provider-id>"
model = "<provider-id>/<model-id>"
api_key_ref = "<provider-api-key-ref>"
```

`[agent].model` is for model-only agents. `[agent.provider]` is for main provider-backed agents. `[agent.subagent.provider]` is OpenCode-only and maps to `opencode.json` `small_model`.

OpenCode init defaults `small_model` to the main model unless the operator declines. `acps subagent disable` stores `invalid/model`; an empty string still triggers OpenCode's implicit fallback.

Do not pass browser OAuth sessions or account cookies through `acp-stack` config or secrets.

For an agent that is not in the embedded registry (private fork, unreleased build), declare `[agent.install] type = "shell"` as an escape hatch with a free-form install script and a `creates` postcheck. The registry-driven install path is implicit when `[agent.install]` is omitted.

## Import And Export

Config import validates TOML before applying it. Config export returns secret references only.

Related commands are defined in [cli](cli.md):

- `acps config validate [path]`
- `acps config export [--output path]`
- `acps config export --base64`
- `acps config import <path> [--force] [--dry-run]`
- `acps config import --base64 <code> [--force] [--dry-run]`

The current implementation supports validation, export, and import:

- `acps config validate [path]` loads an explicit path or `~/.config/acp-stack/acp-stack.toml`.
- `acps config export [--output path]` loads the default config and emits canonical TOML. Export always writes `config_version = 1`.
- `acps config export --base64` emits the same canonical TOML as base64.
- `acps config import <path>` parses, validates, and atomically writes canonical TOML to `~/.config/acp-stack/acp-stack.toml`. Without `--force`, it refuses to overwrite an existing config. `--base64 <code>` decodes its argument as base64-encoded canonical TOML before validation. Atomic writes use a temp-file + rename under owner-only mode (`0600`).
- `acps config import --dry-run` (for both path and `--base64` imports) validates the incoming config, canonicalizes it, compares auth refs against the existing config when present, and reports metadata (config version, canonical size, auth-ref status, target path) without writing to disk or recording an audit event.
- Import input size (raw TOML or decoded base64) is capped at 1 MiB. Oversized imports are rejected before parsing.
- Config version validation: the top-level `config_version` field must be `1`. The field may be omitted in config files, which is treated as version 1 for backward compatibility. Export always emits `config_version = 1`. Unsupported version values are rejected at load time.
- Secret ref field hardening: fields that should contain secret reference names (like `OPENCODE_API_KEY`) are checked for likely inline secret values. Known token prefixes, JWT-shaped values, long hex-only strings, and names exceeding 128 characters are rejected as likely pasted values rather than reference names.
- Optional path fields that are config paths (notably `acpctl.socket_path`) require an absolute path with no `..` segments.
- Validation rejects unknown fields, invalid enum values, relative workspace paths, incomplete or mistyped `[[workspace.code_sources]]` / `[[workspace.data_sources]]` entries, fields that do not belong to the selected source type, and aliased or empty `[auth].session_key_ref` / `[auth].admin_key_ref`.

The Phase 4 init flow accepts two parallel workspace ingestion lanes:

- `[[workspace.code_sources]]` clones Git repositories into
  `<workspace.root>/usr/code/<repo-name>/`. Today only `type = "git"` is
  supported; `repo` is required; `branch`, `credential_ref`, and `name`
  (destination override) are optional.
- `[[workspace.data_sources]]` materializes one of:
  - `type = "local"`: copies an absolute host path tree into
    `<workspace.root>/usr/data/<name>/`. Symlinks under the source are
    refused.
  - `type = "https"`: streams an `https://` URL (Drive/Dropbox/arbitrary
    public hosts) through a redirect-capped, size-capped downloader, then
    extracts safe tar/tar.gz/zip archives or drops the raw payload at
    `<dest>/<basename>` for non-archive responses. `expected_sha256`,
    `max_download_bytes`, and `max_extracted_bytes` are optional caps.
  - `type = "s3"`: paginates `ListObjectsV2` under `bucket`/`prefix`
    using a minimal SigV4 client built from `access_key_ref` +
    `secret_key_ref` secrets, downloading each object beneath the
    per-source byte cap. Path-style endpoints only; overriding the
    endpoint for local mocks is supported via the
    `ACP_STACK_S3_ENDPOINT_OVERRIDE` env var.

Code-source destinations default to the repository leaf name (`.git`
suffix stripped). Data-source destinations default to the local
directory's basename, the archive name (with archive suffix stripped),
or the S3 prefix terminal. `name = "..."` overrides the derived name.

Init refuses to merge into a non-empty destination directory unless the
existing `.acp-stack-source.json` sentinel matches the configured
source. When a sentinel is present and matches, the source is reused
without re-fetching; when it is missing and the directory is non-empty,
init hard-fails with `workspace.destination_not_empty`.

Init does not infer model config from API-key refs. It may write provider/auth config after provider selection and required ref collection. `acps agent set` writes supported model config after the model is explicit. `acp-stack` does not store provider API key values in plaintext.

Phase 4 expands this into a unified provider/model API that resolves provider ids through the provider/env mapping, validates selected model and mode values against ACP session config options, updates the agent-owned config file, and exposes a manual supervised-agent restart path for changes that require process-level reload.

## Request Size Limits

Both `[api].max_request_bytes` and `[security.http].max_request_bytes` cap inbound HTTP request bodies. They are independent fields so that an operator can tighten security limits without changing the headline `[api]` cap, or vice versa. When both are present, the runtime enforces the tighter of the two — `min([api].max_request_bytes, [security.http].max_request_bytes)`. Oversized requests are rejected with 413 before any route handler runs.

`[workspace].max_file_bytes` is a separate, per-file ceiling that the workspace API applies to reads, writes, uploads, and downloads. It is independent of the HTTP body cap because workspace operations also include reads and downloads (which the HTTP body cap does not see) and because the natural file-size limit can be lower than the body cap. Files larger than this limit cannot be transferred through the workspace API regardless of how the bytes are framed on the wire.

## Hardening

Config import/export hardening (Phase 4):

- `config_version = 1` at the top level; missing version treated as 1 for backward compatibility; export always emits version 1.
- Reject unsupported config_version values.
- `acps config import --dry-run` and `POST /v1/config/import?dry_run=true` validate, canonicalize, compare auth refs, and report metadata without writing or auditing.
- Shared 1 MiB import-size cap for CLI and API; oversized API input returns 413.
- Validate optional path fields that are config paths (notably `acpctl.socket_path`): absolute path required and no `..` segments.
- Reject imported config where secret-reference fields contain likely inline secret values (known token prefixes, JWT-shaped values, long hex-only strings, names > 128 chars) while allowing normal refs like `OPENCODE_API_KEY`.
- Preserve existing auth-ref import protections and canonical export behavior.
