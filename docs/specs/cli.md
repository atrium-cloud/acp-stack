# CLI

`acps` is the local command-line interface for initializing, running, inspecting, and operating an `acp-stack` instance.

The CLI should call the same core service layer as the HTTP API where practical. It should not grow a separate behavior path that diverges from the daemon.

## Commands

Initial CLI commands:

```sh
acps init
acps serve
acps status
acps reset --yes

acps auth regenerate-session-key

acps agent install
acps agent start
acps agent stop
acps agent status

acps config validate [path]
acps config export [--output path]
acps config export --base64
acps config import <path>
acps config import --base64 <code>

acps secrets list
acps secrets set <name>
acps secrets delete <name>

acps sessions list
acps sessions new
acps sessions prompt <session-id>
acps sessions cancel <session-id>
acps sessions close <session-id>

acps logs tail
acps logs query
acps security check
acps deps check
acps deps apply
```

## Auth Commands

`acps init` generates both API keys on first run and stores them in the age-encrypted secret store under the names declared by `[auth].session_key_ref` and `[auth].admin_key_ref`. Both values are printed once on stdout during that first run; subsequent `acps init` invocations preserve the existing keys and do not reveal them again.

`acps auth regenerate-session-key` rotates the general session key and prints the new value. The admin key is generated only once during init and is not regenerable in place; use `acps reset --yes` to wipe and re-init if the admin key is lost or compromised.

## Reset

`acps reset --yes` is the disposable-instance reset path. It deletes `~/.config/acp-stack/acp-stack.toml`, `~/.local/share/acp-stack/state.sqlite`, `~/.config/acp-stack/age.key`, and `~/.local/share/acp-stack/secrets.age`, leaves their parent directories in place, and is idempotent against already-missing files. Without `--yes`, `acps reset` prints the deletion plan and exits non-zero without touching the filesystem. `acps reset` is the only way to rotate the admin key.

## Init

`acps init` creates or validates local config and state, initializes the age-encrypted secret store, and generates the two API keys named by `[auth]`. Init may run the configured agent installer after explicit user confirmation once installer execution exists.

`acps init` can seed the workspace from one source:

- `none` - start with an empty workspace and upload work data later
- `git` - clone a repository into the workspace
- `s3` - download or sync an S3 bucket/prefix into the workspace

Git sources may reference a credential secret for private repositories. S3 sources should reference AWS credential secrets instead of embedding credentials in config.

## Serve

`acps serve` runs the HTTP daemon in the foreground. It blocks the calling shell until it receives `SIGTERM` or `SIGINT`, at which point it triggers a graceful shutdown and exits. The expected deployment is to run it under a process manager (`systemd`, `launchd`, supervisord, a container init) — `acps` itself does not daemonize, does not write a PID file, and does not fork. Standard error carries the startup and shutdown announcement; structured runtime history goes to the SQLite `agent_lifecycle` table as `server.starting`, `server.started`, and `server.stopped` rows.

Bind defaults to `[api].bind` from config (`127.0.0.1:7700`). `--bind <addr>` overrides it for this run. The HTTP server enforces the request body cap as `min([api].max_request_bytes, [security.http].max_request_bytes)` and 413s oversized requests before any handler runs.

`acps serve` requires both API keys to already exist in the encrypted secret store under the names declared in `[auth]`; missing keys fail startup before the listener binds.

## Agent Commands

`acps agent install` installs the configured ACP agent process. The operator-facing path resolves the agent or adapter from the ACP registry, then records the installer run in SQLite, verifies `creates`, and checks `expected_sha256` when configured. Adapter-backed agents install the adapter executable, such as `codex-acp`, because that is the process `acp-stack` speaks ACP with. Direct shell recipes are a low-level/manual escape hatch, not the preferred discovery or installation path.

`acps agent start` and `acps agent stop` call the running daemon over HTTP using the admin key from the encrypted secret store. The base URL is `[api].public_url` when configured; otherwise it is derived from `[api].bind`, with wildcard binds rewritten to loopback for local CLI calls.

`acps agent status` reads local config and SQLite state, including the latest persisted capability snapshot and recent lifecycle rows.

## Security Self-Check

`acps security check` runs the local self-check described in [security](security.md).

## Local Agent Interface

`acpctl` is separate from `acps`. It is the constrained local, agent-facing interface described in [acpctl](acpctl/acpctl.md).

## Current 0.0.1 Implementation Subset

The first implemented CLI surface focuses on local config, durable state, the secret store, and the foreground HTTP daemon. `init`, `status`, and `logs query` all create or migrate the local SQLite file when missing:

- `acps --version`
- `acps config validate [path]`
- `acps config export [--output path]`
- `acps config export --base64`
- `acps config import <path> [--force]`
- `acps config import --base64 <code> [--force]`
- `acps init`
- `acps status`
- `acps reset [--yes]`
- `acps auth regenerate-session-key`
- `acps secrets list`
- `acps secrets set <name>`
- `acps secrets delete <name>`
- `acps agent install`
- `acps agent start`
- `acps agent stop`
- `acps agent status`
- `acps logs query [--limit <n>] [--level <level>] [--since <duration|rfc3339>] [--until <duration|rfc3339>] [--kind <kind|prefix.>] [--source <writer>] [--session <id>] [--command <id>] [--permission <id>] [--after <cursor>]`
- `acps logs tail [--topic <name>]...`
- `acps metrics summary [--since <duration|rfc3339>] [--until <duration|rfc3339>]`
- `acps serve [--bind <addr>]`

When `[path]` is omitted for validation, the CLI reads `~/.config/acp-stack/acp-stack.toml`. Export currently reads the same default path and writes canonical TOML to stdout unless `--output` is provided.

`acps init` creates the default config and state directories, writes a valid starter config when one is absent, validates an existing config without overwriting it, creates or migrates `~/.local/share/acp-stack/state.sqlite`, initializes the age key and the encrypted secret store, generates session and admin API keys when the store is fresh, and records `init.completed` and `auth.keys_generated` events. On a re-run with both API keys already present, init preserves them silently; if either reference name is missing in a non-empty store, init fails fast.

`acps config import` validates the incoming TOML and writes it to the default config path as canonical TOML. By default, import refuses to overwrite an existing config; pass `--force` to replace one. `--base64 <code>` decodes its argument as base64-encoded canonical TOML before validation.

`acps secrets set <name>` reads a single line from stdin and stores it as the named secret. `acps secrets list` prints names only — values are never echoed. `acps secrets delete <name>` removes the named secret and errors when it does not exist.

`acps status` validates the default config, opens or migrates local state, records `status.checked`, and prints config, state, schema version, and latest event status.

`acps logs query` reads durable SQLite events newest-first. `--limit` defaults to `50`. Additional filters: `--level <level>` (exact match); `--kind <kind>` (exact, or dotted prefix when the value ends with `.`); `--source <writer>` (`api`/`acp`/`command`/`permission`/`cli`/`system`); `--session <id>`, `--command <id>`, `--permission <id>` for cross-reference lookups; `--since` and `--until` accept either an RFC3339 timestamp or a duration suffix (`30m`, `1h`, `2d`, `1w` — interpreted as "this much time ago"); `--after <event-id>` continues a keyset-paginated scan past the previous page's last row. Each output line is `<created_at> <level> <source> <kind> <message>`.

`acps metrics summary` calls `/v1/metrics/summary` on the running daemon and pretty-prints the JSON response. Without `--since` the window defaults to 24h; the same duration/RFC3339 form as `logs query` is accepted.

`acps logs tail` opens a WebSocket subscription to the running daemon and prints each frame as it arrives until SIGINT. `--topic <name>` may be repeated to subscribe to multiple topics; the default is `logs`. Authentication uses the session key from the encrypted secret store, so the daemon must be reachable at `[api].public_url` (or the loopback rewrite of `[api].bind`).
