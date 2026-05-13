# Security

Security boundaries span API authentication, secret storage, permission review, HTTP hardening, deployment posture, and local runtime ownership.

## API Keys

The runtime uses two API keys:

- session key
- admin key

Keys are generated during `acps init`. They are stored as secrets and referenced by config.

The session key authorizes general operations and can be regenerated with `acps auth regenerate-session-key`.

The admin key authorizes elevated operations. It is generated only once during init. If it is lost or compromised, the best course of action is to export data and config, then shut down the instance.

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

0.0.2 uses age-compatible encryption:

- private key: `~/.config/acp-stack/age.key`
- encrypted store: `~/.local/share/acp-stack/secrets.age`
- public key used for new secret encryption

Secret references appear in config:

```toml
[agent]
env = ["OPENCODE_API_KEY"]

[logging.supabase]
service_role_key_ref = "SUPABASE_SERVICE_ROLE_KEY"

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
- the Supabase sink receives only the configured `service_role_key_ref` when external logging is enabled
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
timeout = "5m"
timeout_action = "deny"
review = ["rm *", "git push *", "chmod *", "curl * | sh", "sudo *"]
deny = ["shutdown", "reboot"]
```

Modes:

- `auto` - allow most mediated operations, review or deny matching patterns
- `supervised` - review destructive mediated operations
- `locked` - require approval for every mediated command

Important 0.0.x boundary:

The Command Gateway controls commands launched through `acp-stack` and terminal capabilities that `acp-stack` mediates. It does not claim to intercept arbitrary process activity outside its control path.

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
