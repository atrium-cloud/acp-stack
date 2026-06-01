# CLI

`acps` is the operator CLI for initializing, running, and inspecting an `acp-stack` instance. `acpctl` is the constrained local interface for agents and local shell users; see [acpctl](acpctl/acpctl.md).

## Command Groups

| Area           | Commands                                                                    |
| -------------- | --------------------------------------------------------------------------- |
| Instance       | `acps init`, `acps serve`, `acps status`, `acps reset --yes`                |
| Auth           | `acps auth regenerate-session-key`                                          |
| Config         | `acps config validate`, `export`, `import`                                  |
| Secrets        | `acps secrets list`, `set`, `delete`                                        |
| Agents         | `acps agent install`, `switch`, `start`, `stop`, `restart`, `status`, `check`, `test` |
| Provider/model | `acps agent set`, `acps subagent status/set/match/free/disable`             |
| Sessions       | `acps sessions list/status/new/fork/prompt/cancel/close`                    |
| Logs/metrics   | `acps logs query`, `logs tail`, `metrics summary`                           |
| Operations     | `acps deps check`, `deps apply`, `security check`, `security history`, `security show`, `installer history` |
| WebSockets     | `acps ws connections`, `ws sessions`, `ws disconnect`                       |
| Shell          | `acps completion <shell>`                                                   |

Commands read `~/.config/acp-stack/acp-stack.toml` by default unless an explicit path argument is documented.

Most operator commands accept global `--format text|json`; text is the default. Commands that remain text-only reject `--format json` instead of silently ignoring it. Existing `--json` flags remain accepted as aliases for `--format json` and conflict with explicit `--format text`. `acps logs query --follow --format json` emits newline-delimited event objects.

## Initialization

`acps init` creates or validates config and state, initializes the encrypted secret store, generates API keys on first run, and optionally configures an agent, provider, workspace sources, MCP servers, edge profile, and testflight.

Common flags:

```sh
acps init \
  [--agent <id>] [--non-interactive] \
  [--skills-source <openai|anthropic|github:owner>] [--skills <name,name>|--no-skills] \
  [--provider <provider-id>] [--api-key-ref <ref>] [--model <model-id>] [--mode <mode>] \
  [--custom-provider --provider <id> --provider-name <name> --base-url <url> --api-key-ref <ref> --model <model-id>] \
  [--workspace-root <path>] [--workspace-uploads <path>] [--runtime-user <name>] \
  [--code-from <repo-url>]... [--data-from <path-or-url>]... \
  [--mcp-preset linear] [--mcp-stdio <name=command>]... [--mcp-stdio-env <server=SECRET_REF>]... \
  [--mcp-http <name=https://...>]... [--mcp-http-header <server=Header:SECRET_REF>]... \
  [--supabase-url <url>] [--supabase-schema <schema>] [--supabase-api-key-ref <ref>] [--no-supabase] \
  [--edge cloudflare --exposure tunnel --hostname <host>] [--cloudflare-mode generated|managed] \
  [--cloudflare-api-token-ref <ref> --cloudflare-account-id-ref <ref>] \
  [--testflight|--skip-testflight] [--resume [--run-id <id>] | --fresh]
```

Interactive init may prompt for missing choices. Non-interactive first runs require `--agent <id>`; scripts should pass `--non-interactive` with the selected real agent id. Provider-backed setup also requires explicit flags and existing secret refs. Re-running init preserves existing API keys and config unless an explicit option requests a fresh run.

`--workspace-root`, `--workspace-uploads`, and `--runtime-user` affect only a new starter config. Once config exists, contradictory deployment overrides are rejected.

`acps init` creates or validates the workspace root and uploads directory, then installs the configured real agent. Adapter-backed agents install both the harness and adapter. `--code-from` appends Git code sources to a new starter config. `--data-from` appends local or HTTPS data sources. Plain HTTP URLs are rejected.

On new starter configs, `--mcp-preset linear` adds the Linear hosted MCP declaration using `LINEAR_API_KEY`. `--mcp-stdio name=command` and `--mcp-http name=https://...` add custom runtime-wide MCP declarations. `--mcp-stdio-env server=SECRET_REF` and `--mcp-http-header server=Header:SECRET_REF` attach required secret refs to those declarations.

`--supabase-url` enables the external Supabase logging sink during init. `--supabase-schema` defaults to `acp_stack`; `--supabase-api-key-ref` defaults to `SUPABASE_SECRET_KEY`. Interactive init prompts for a missing Supabase secret key. Non-interactive init expects `ACP_STACK_SUPABASE_SECRET_KEY` or an existing secret-store entry.

`--skills-source` and `--skills` install selected Agent Skills before testflight.
Official sources are `openai` and `anthropic`; custom sources use
`github:<owner>` and expect `<owner>/skills` on branch `main`.

Cloudflare `generated` mode writes tunnel artifacts for operator-managed setup. Cloudflare `managed` mode uses the configured secret refs to create the tunnel, push the ingress config, create or update the proxied CNAME, and emit an owner-only tunnel token env artifact during init.

## Auth And Reset

First initialization prints two API keys:

- Session key: normal sessions, workspace, commands, logs, and status.
- Admin key: secrets, config import, agent process control, and other elevated
  operations.

`acps auth regenerate-session-key` rotates only the session key. The admin key is not regenerated in place; `acps reset --yes` destroys local config, state, age key, and secret store so a new instance can be initialized.

## Config Commands

```sh
acps config validate [path]
acps config export [--output path]
acps config export --base64
acps config import <path> [--force] [--dry-run]
acps config import --base64 <code> [--force] [--dry-run]
```

Export emits canonical TOML with secret references only. Import validates and canonicalizes TOML before writing it. Without `--force`, import refuses to replace an existing config. `--dry-run` reports what would change without writing.

## Logging Commands

```sh
acps logging supabase status
acps logging supabase setup --url <url> [--project-ref <ref>] [--yes]
acps logging supabase check [--format json]
acps logging supabase sql
acps logging supabase enable --url <url> [--schema <schema>] [--api-key-ref <ref>]
acps logging supabase disable
acps logging supabase set-secret [--api-key-ref <ref>]
acps logging supabase set-db-url [--db-url-ref <ref>]
```

`setup` uses the Supabase CLI to provision table-backed logging, then stores only the narrow runtime writer DB URL in the encrypted secret store. `check` writes a marked canary row to prove the configured backend can receive logs. `set-secret` remains for the legacy PostgREST backend. Status output reports whether configured secrets exist but never prints their values.

## Agent Commands

`acps agent install [--yes]` installs the configured supported agent from the embedded catalog. Unsupported entries fail before installation. `--yes` is accepted for scripts; install currently runs non-interactively.

`acps agent switch <agent>` migrates to another supported harness through the running daemon:

```sh
acps agent switch <agent> [--drop] [--provider <provider-id>] [--api-key-ref <ref>] [--admin-key <key>]
```

The target agent is positional. Non-interactive runs require `--admin-key`; interactive runs prompt for the admin key without echoing it. Before calling the daemon, switch prints the target install steps, config that will migrate as-is, compatible provider secret refs that will be copied if missing, optional source config cleanup, and fields that need input. Switch installs the target harness, reuses the current provider/API-key ref only when compatible, copies installed Agent Skills into the target skills directory when needed, clears the model, and prints advertised model values only when the target supports model selection. Interactive runs can select and apply a model before the command exits. Non-interactive runs print `acps agent set --model <model-id>` as the follow-up only when model selection is supported.

Switch preserves runtime-scoped config, including workspace, MCP declarations, permissions, auth, secrets config, and sessions. By default, it also preserves source agent-owned config, secrets, and installed harnesses/adapters so switching back is fast. `--drop` removes only source agent-owned config after the target switch succeeds. It does not delete runtime MCP declarations, secrets, binaries, adapters, or sessions.

`acps agent set` updates provider, model, mode, and custom-provider metadata:

```sh
acps agent set --provider <provider-id> [--model <model>] [--api-key-ref <ref>]
acps agent set --custom-provider --provider <id> --provider-name <name> --base-url <url> --api-key-ref <ref> --model <model-id>
acps agent set --model <model>
acps agent set --mode <mode>
```

Mapped model and mode values are validated against the configured agent's ACP-advertised options. Custom-provider model ids are accepted as supplied. For provider-backed agents, `acps agent set --model <model>` uses the existing `[agent.provider]` when present. When a change requires the supervised process to reload agent-owned config, the CLI prints a restart hint.

`acps subagent *` is OpenCode-only and manages the OpenCode small-model lane. `acps subagent match` makes `small_model` follow the main agent model.

`acps agent start`, `stop`, and `restart` call the running daemon with the admin key. `acps agent status` prints configured identity, process state, capability summary, and recent lifecycle information. `acps agent check` reports whether managed install steps are present and current.

`acps agent test` sends a real prompt through the configured agent. It may use provider credits and should be run only when that is intentional.

## Sessions

```sh
acps sessions list [--range <day|week|month|year|all|duration>] [--range-start <datetime>] [--range-end <datetime>] [--limit <n>]
acps sessions status [--threshold <duration>] [--limit <n>]
acps sessions new
acps sessions fork <session-id> [--message-id <id>] [--cwd <path>]
acps sessions prompt <session-id>
acps sessions cancel <session-id>
acps sessions close <session-id>
```

`sessions list` shows the durable local session list after any supported ACP session-list sync. Sessions discovered from the agent but not loaded locally are shown as `available`.

`sessions status` prints active sessions with recent or idle state. The default recent threshold is `15m`.

`sessions fork` creates a child session through ACP. `--message-id` forks from an acknowledged prompt message id when the agent advertises that capability.

## Logs, Metrics, And Health

`acps status` validates local config and state, prints workspace and agent status, and probes daemon readiness when the daemon is reachable.

`acps logs query` reads durable events. Filters include level, kind or kind prefix, source, session id, command id, permission id, security category, time bounds, and cursor. `--order <asc|desc>` flips sort direction (default `desc`). `--json` emits the `{ events, next_cursor }` envelope to stdout and suppresses the human "more rows" hint. `--category <rate_limit|origin_cors|ip_block|oversized_request>` scopes to one security category. `--follow` subscribes to the daemon's `logs` WebSocket topic, drains matching durable backlog in ascending pages, then continues with live events. With `--json --follow`, stdout is newline-delimited `EventJson` objects rather than the non-follow envelope.

`acps logs tail` opens a WebSocket subscription to the running daemon.

`acps metrics summary` prints the daemon's summary metrics for a time window.

`acps security check` runs the security self-check and persists the run to history. `acps security history [--limit N] [--after <id>] [--json]` lists prior runs newest-first; `acps security show <run-id> [--json]` prints a single recorded run with its findings. `acps deps check` reports declared dependency status. `acps deps apply` runs only install actions declared in config and requires confirmation unless `--yes` is passed.

## Shell Completion

`acps completion <bash|zsh|fish|powershell|elvish>` writes a completion script to stdout.
