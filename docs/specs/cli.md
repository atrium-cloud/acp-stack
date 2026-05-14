# CLI

`acps` is the local command-line interface for initializing, running, inspecting, and operating an `acp-stack` instance.

The CLI should call the same core service layer as the HTTP API where practical. It should not grow a separate behavior path that diverges from the daemon.

## Commands

Initial CLI commands:

```sh
acps init
acps serve
acps status

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

The full auth implementation will generate both API keys during `acps init`. The current 0.0.1 init subset defers API key generation until secret storage exists.

`acps auth regenerate-session-key` rotates the general session key. The admin key is generated only once during init and is not regenerable.

## Init

`acps init` creates or validates local config and state. Secret storage and API key generation are added with the auth/secrets implementation. Init may run the configured agent installer after explicit user confirmation once installer execution exists.

`acps init` can seed the workspace from one source:

- `none` - start with an empty workspace and upload work data later
- `git` - clone a repository into the workspace
- `s3` - download or sync an S3 bucket/prefix into the workspace

Git sources may reference a credential secret for private repositories. S3 sources should reference AWS credential secrets instead of embedding credentials in config.

## Security Self-Check

`acps security check` runs the local self-check described in [security](security.md).

## Local Agent Interface

`acpctl` is separate from `acps`. It is the constrained local, agent-facing interface described in [acpctl](acpctl/acpctl.md).

## Current 0.0.1 Implementation Subset

The first implemented CLI surface focuses on local config and durable state — no network operations and no agent supervision yet. `init`, `status`, and `logs query` all create or migrate the local SQLite file when missing:

- `acps --version`
- `acps config validate [path]`
- `acps config export [--output path]`
- `acps config export --base64`
- `acps init`
- `acps status`
- `acps logs query [--limit <n>] [--level <level>]`

When `[path]` is omitted for validation, the CLI reads `~/.config/acp-stack/acp-stack.toml`. Export currently reads the same default path and writes canonical TOML to stdout unless `--output` is provided.

`acps init` creates the default config and state directories, writes a valid starter config when one is absent, validates an existing config without overwriting it, creates or migrates `~/.local/share/acp-stack/state.sqlite`, and records an `init.completed` event. API key and secret generation are deferred to the auth/secrets implementation.

`acps status` validates the default config, opens or migrates local state, records `status.checked`, and prints config, state, schema version, and latest event status.

`acps logs query` reads durable SQLite events newest-first. `--limit` defaults to `50`, and `--level` filters by exact event level.
