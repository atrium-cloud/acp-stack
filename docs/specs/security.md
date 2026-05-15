# Security

Security boundaries span API authentication, secret storage, permission review, HTTP hardening, deployment posture, and local runtime ownership.

## API Keys

The runtime uses two API keys:

- session key
- admin key

`acps init` generates both keys, stores them in the age-encrypted secret store under the names declared by `[auth].session_key_ref` and `[auth].admin_key_ref` in the config, and prints the values once on stdout. The admin key is printed only on the run that generates it; re-running `acps init` against an already-initialized instance preserves both keys and does not reveal the admin value again. Keys are formatted as `acps_<43-char base64url>` (32 random bytes from the system CSPRNG, base64url-no-pad, with the `acps_` prefix).

The session key authorizes general operations and can be regenerated with `acps auth regenerate-session-key`.

The admin key authorizes elevated operations. It is generated only once during init and is never regenerable in place. If it is lost or compromised, the operator must run `acps reset --yes` to wipe config, state, age key, and the secret store, then re-run `acps init` to regenerate a fresh pair. `acps init` fails fast if the secret store exists but is missing the admin key reference, treating that state as an anomaly that requires the operator to investigate.

## API Key Tiering

The HTTP layer applies strict tiering between the two keys: the admin key authorizes management functions and destructive / runtime-state-altering actions (e.g. secrets mutations, config import, agent install/start/stop, key rotation). The session key authorizes everything else (status reads, config export, config validate, log queries, sessions, prompts, workspace operations, command runs, permission decisions). Normal session operations stay session-tier even when they write rows.

Strict tiering means the admin key is rejected on session-tier routes (`auth.wrong_kind`, 401) and the session key is rejected on admin-tier routes. There is no superset relationship. This isolates the higher-blast-radius credential to the smallest set of routes that need it.

Both keys are presented as `Authorization: Bearer <key>`. The server determines which key was presented by constant-time comparing against both stored values, then enforces the route's required tier in a per-route middleware.

## Auth Failure Logging

Every rejected authentication is recorded as a row in the `auth_failures` table. Rows capture the request route, the resolved key kind (`session`, `admin`, or `unknown` when no match), and the reason code (`missing`, `malformed_header`, `invalid`, `wrong_kind`). The attempted token value is never stored; only the kind that was expected and why it failed. In 0.0.1 rows carry the socket client IP when available; proxy-header interpretation behind `security.http.trust_proxy_headers` belongs with the later proxy hardening work. Rows also carry a small JSON payload for forward-compatible context (rate-limit hints, etc.).

## Baseline Posture

- supports bearer key auth
- supports request size limits
- validates API keys with constant-time comparison
- logs failed authentication without storing attempted key values
- applies per-IP and per-key rate limits to HTTP routes
- applies lower unauthenticated request limits before a key is accepted
- temporarily blocks IPs after repeated authentication failures
- validates WebSocket `Origin` against the configured allowlist when present
- supports CORS only for configured origins
- rejects oversized request bodies before route handlers process them
- runs as an unprivileged Linux user by default, normally `acp`
- spawns the agent, MCP servers, and mediated shell commands as the same unprivileged runtime user
- stores config, state, age key, and encrypted secret store under paths owned by the runtime user with owner-only permissions
- binds to `127.0.0.1:7700` by default
- assumes TLS and public-edge hardening are handled by a reverse proxy for internet exposure
- keeps TLS termination and persistent IP allowlists out of the 0.0.x core
- treats root execution as an explicit disposable/dev deployment profile, not the standard deployment model

Deployment guidance should document reverse proxy patterns for Caddy, Nginx, Fly, Railway, and Hetzner.

## Secrets

Secrets are stored outside the portable config.

0.0.1 uses age-compatible encryption:

- private key: `~/.config/acp-stack/age.key` (bech32 x25519 identity, owner-only `0600`)
- encrypted store: `~/.local/share/acp-stack/secrets.age` (owner-only `0600`)
- the store is encrypted to its own public key; the inner plaintext is a TOML document of the form `[secrets]\nNAME = "value"`
- mutations rewrite the full ciphertext through an atomic temp-file rename

Secret references appear in config:

```toml
[agent]
env = ["OPENCODE_API_KEY"]

[logging.supabase]
api_key_ref = "SUPABASE_SECRET_KEY"

[[mcp.servers]]
name = "slack"
env = ["SLACK_BOT_TOKEN", "SLACK_TEAM_ID"]

[[mcp.servers]]
name = "linear"
env = ["LINEAR_API_KEY"]
```

Secret values are managed by CLI or API:

```sh
acps secrets set OPENCODE_API_KEY
acps secrets list
acps secrets delete OPENCODE_API_KEY
```

Scoped injection rules:

- the agent receives only names listed in `[agent].env`
- an MCP server receives only names listed in its `env`
- HTTP MCP headers may interpolate referenced secrets
- the Supabase sink receives only the configured `api_key_ref` when external logging is enabled
- the full secret store is never injected into any child process
- secret values are never returned by API or config export

Security limitation:

If an attacker has root on a running instance, process environment variables may be readable through OS facilities. Age protects secrets at rest and in exported configs; it cannot protect secrets from full runtime compromise.

## Permissions

0.0.2 implements the permission pipeline as a durable product primitive.

Permission sources:

- ACP `session/request_permission`
- commands launched through `POST /v1/commands`
- future runtime modules

Permission lifecycle:

1. Runtime creates a permission request.
2. Request is persisted in SQLite.
3. Request is pushed over WebSocket to subscribed clients.
4. Request is visible through `GET /v1/permissions/pending`.
5. User approves or denies over HTTP or WebSocket.
6. Decision is persisted.
7. Runtime resumes, rejects, or times out the blocked operation.

Policy example:

```toml
[permissions]
mode = "auto"
review = ["rm *", "git push *", "chmod *", "curl * | sh", "sudo *"]
deny = ["shutdown*", "reboot*"]
```

Patterns are full-string shell-style globs matched against the raw command line. `*` matches any sequence of characters (including the empty string); `?` matches exactly one. They are anchored at both ends, so a bare `shutdown` matches only the literal `shutdown` and not `shutdown now` — explicit args should be covered with `shutdown*` or `shutdown *`. Patterns do not currently introspect shell composition (`;`, `&&`, pipelines), so `true; shutdown now` would not be matched by `shutdown*` — denylist coverage of shell composition is deferred to the permissions module.

The `timeout` and `timeout_action` knobs documented here in earlier drafts are reserved for the permissions module that owns the approval queue; the phase-1 config schema does not accept them yet.

Modes:

- `auto` - allow most mediated operations, review or deny matching patterns
- `supervised` - review destructive mediated operations
- `locked` - require approval for every mediated command

Important 0.0.x boundary:

The Command Gateway controls commands launched through `acp-stack` and terminal capabilities that `acp-stack` mediates. It does not claim to intercept arbitrary process activity outside its control path.

0.0.1 enforces only the static `deny` and `review` glob lists at submission time. There is no pending-approval queue, no `permissions` WebSocket topic producer, and no `/v1/permissions/...` routes — those land with the dedicated permissions module. Until then, `review` matches behave like `deny` when `mode` is `supervised` or `locked`, and emit a `command.review_flagged` event while proceeding when `mode = "auto"`.

## Security Self-Check

0.0.4 adds a self-check command:

```sh
acps security check
```

The same check is exposed through:

```http
GET /v1/security/check
```

The self-check inspects local configuration, file permissions, runtime status, and recent security events. It returns a severity-ranked report with `ok`, `warning`, and `critical` findings.

Checks include:

- API bind address is not unexpectedly public.
- Reverse proxy assumptions are explicit when `api.bind` is public.
- API keys exist and are not default/empty.
- Config, state, age key, and encrypted secret store are owner-only readable.
- Runtime is not running as root unless disposable/dev profile is explicitly configured.
- Workspace is owned by the runtime user.
- Agent binary hash matches `expected_sha256` when configured.
- Agent installer command has not changed since last successful install.
- Recent auth failure rate is below configured thresholds.
- Temporary IP blocks are not spiking.
- CORS and WebSocket origin allowlists are not wildcarded on public binds.
- `trust_proxy_headers` is not enabled without trusted proxy configuration.
- Supabase logging failures are not silently accumulating.
- No recent command or permission events match high-risk patterns without review.

Example output shape:

```json
{
  "ok": false,
  "summary": {
    "critical": 1,
    "warning": 2,
    "ok": 12
  },
  "findings": [
    {
      "id": "agent.hash_mismatch",
      "severity": "critical",
      "message": "Configured agent binary does not match expected_sha256.",
      "remediation": "Reinstall the agent or update expected_sha256 after verifying the binary."
    }
  ]
}
```

## Runtime User

Recommended Linux layout:

```text
user: acp
home: /home/acp
workspace: /workspace
config: /home/acp/.config/acp-stack
state: /home/acp/.local/share/acp-stack
```

Host setup:

```sh
useradd --create-home --shell /bin/bash acp
mkdir -p /workspace
chown -R acp:acp /workspace
```

Systemd units should set:

```ini
User=acp
Group=acp
WorkingDirectory=/workspace
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=/workspace /home/acp/.config/acp-stack /home/acp/.local/share/acp-stack
```

Docker images should use `USER acp` and mount `/workspace` writable by that UID.
