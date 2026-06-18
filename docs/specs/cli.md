# CLI

`acps` is the CLI for initializing, running, inspecting, and operating an `acp-stack` instance.

## Command Groups

| Area           | Commands                                                                    |
| -------------- | --------------------------------------------------------------------------- |
| Instance       | `acps init`, `acps serve`, `acps status`, `acps update`, `acps reset --yes` |
| Auth           | `acps auth regenerate-session-key`                                          |
| Config         | `acps config validate`, `export`, `import`                                  |
| Secrets        | `acps secrets list`, `set`, `delete`                                        |
| Agents         | `acps agent install`, `update`, `switch`, `start`, `stop`, `restart`, `status`, `check`, `test` |
| Provider/model | `acps agent set`, `acps subagent status/set/match/free/disable`             |
| Sessions       | `acps sessions list/status/new/load/resume/fork/prompt/cancel/close`        |
| Logs/metrics   | `acps logs query`, `logs tail`, `metrics summary`                           |
| Operations     | `acps deps check`, `deps apply`, `security check`, `security history`, `security show`, `installer history` |
| WebSockets     | `acps ws connections`, `ws sessions`, `ws disconnect`                       |
| Shell          | `acps completion <shell>`                                                   |

Commands read `~/.config/acp-stack/acps-config.toml` by default unless an explicit path argument is documented.

Most operator commands accept global `--format text|json`; text is the default. Commands that remain text-only reject `--format json` instead of silently ignoring it. Existing `--json` flags remain accepted as aliases for `--format json` and conflict with explicit `--format text`. `acps logs query --follow --format json` emits newline-delimited event objects.

## Initialization

`acps init` creates or validates config and state, initializes the encrypted secret store, generates API keys on first run, and optionally configures an agent, provider, workspace sources, MCP servers, edge profile, and testflight. See [init.md](init.md) for the end-to-end interactive flow and step sequence; this section is the flag reference.

Common flags:

```sh
acps init \
  [--from-file <path>|--from-toml <toml>|--from-base64 <base64>] \
  [--agent <id>] [--non-interactive] \
  [--custom-agent-id <id> --custom-agent-command <cmd> --custom-agent-install <shell> \
   [--custom-agent-name <name>] [--custom-agent-arg <arg>]... [--custom-agent-creates <path>]] \
  [--agent-env-ref <name>]... \
  [--dep <name=shell>]... [--dep-system <name=shell>]... [--deps-apply [--deps-apply-yes]] \
  [--stack-update <on|security|off> [--stack-update-frequency <freq>]] \
  [--skills-source <openai|anthropic|github:owner>] [--skills <name,name>|--no-skills] \
  [--provider <provider-id>] [--api-key-ref <ref>] [--model <model-id>] \
  [--custom-provider --provider <id> --provider-name <name> --base-url <url> --api-key-ref <ref> --model <model-id>] \
  [--workspace-root <path>] [--workspace-uploads <path>] [--runtime-user <name>] \
  [--code-from <repo-url>]... [--data-from <path-or-url>]... \
  [--mcp-preset linear] [--mcp-stdio <name=command>]... [--mcp-stdio-env <server=SECRET_REF>]... \
  [--mcp-http <name=https://...>]... [--mcp-http-header <server=Header:SECRET_REF>]... \
  [--supabase-url <url>] [--supabase-schema <schema>] [--supabase-api-key-ref <ref>] [--no-supabase] \
  [--edge cloudflare --exposure tunnel --hostname <host>] [--cloudflare-mode generated|managed] \
  [--cloudflare-api-token-ref <ref> --cloudflare-account-id-ref <ref>] \
  [--testflight|--skip-testflight] [--resume [--run-id <id>] | --fresh] \
  [--handoff-json]
```

Interactive init may prompt for a config source, then for missing choices. Optional setup is selected from one grouped prompt before any selected item asks for details; selecting Skip continues without optional setup. MCP stdio setup collects command, args, and env refs together; MCP HTTP setup collects URL and header refs together. Agent, provider, and advertised model selectors are searchable. `--from-file`, `--from-toml`, and `--from-base64` initialize from an existing `acps-config.toml`; the interactive source prompt offers file import and base64 paste, while `--from-toml` is for scripted raw TOML input. The base64 form is the same TOML content encoded for safer terminal paste. Non-interactive first runs require `--agent <id>`, the `--custom-agent-*` set, or a complete imported config; scripts should pass `--non-interactive` with the selected real agent id, explicit provider flags when provider setup is required, and resolvable secret refs. A custom (non-registry) agent is declared with `--custom-agent-id`, `--custom-agent-command`, and `--custom-agent-install` (optionally `--custom-agent-name`, repeatable `--custom-agent-arg`, and `--custom-agent-creates`); it writes an `[agent.install]` shell escape hatch, is also offered as a "Custom agent…" choice in the interactive picker, and conflicts with the `--provider`/`--model` init flags (custom agents configure those through their own environment). Repeatable `--agent-env-ref <name>` adds extra secret-backed environment variables to `[agent].env` (new config only); the named secret must already resolve in the store, while interactive runs can collect masked values when Agent environment is selected in optional setup. Repeatable `--dep <name=shell>` (user scope) and `--dep-system <name=shell>` (system scope) declare `[dependencies.commands]` install actions (new config only). `--deps-apply` runs the pending install actions during init; it confirms interactively, and non-interactive runs additionally require `--deps-apply-yes`. Apply outcomes are recorded under `acps installer history --agent deps_apply`. `--stack-update <on|security|off>` sets the `[updates.acp_stack]` policy (on = all compatible, security = security-critical only, off = manual); `--stack-update-frequency <freq>` sets the schedule at day/week granularity (minimum 1 day, e.g. `1d`, `3w`) for non-off policies. Omitting both in a non-interactive run keeps the defaults (security-critical, `1d`). Re-running init preserves existing API keys and config unless an explicit option requests a fresh run.

`--workspace-root`, `--workspace-uploads`, and `--runtime-user` affect only a new starter config. Once config exists, contradictory deployment overrides are rejected.

`--handoff-json` is the platform automation output mode for init. It disables prompts and emits only the handoff JSON object described in [init.md](init.md#platform-handoff-json). `acps init --format json` remains rejected; scripts should use `--handoff-json` for this narrower contract.

`acps init` creates or validates the workspace root and uploads directory, then installs the configured real agent. Adapter-backed agents install both the harness and adapter. `--code-from` appends Git code sources to a new starter config. `--data-from` appends local or HTTPS data sources. Interactive init can also collect S3 data sources. Plain HTTP URLs are rejected.

On new starter configs, `--mcp-preset linear` adds the Linear hosted MCP declaration using `LINEAR_API_KEY`. `--mcp-stdio name=command` and `--mcp-http name=https://...` add custom runtime-wide MCP declarations. `--mcp-stdio-env server=SECRET_REF` and `--mcp-http-header server=Header:SECRET_REF` attach required secret refs to those declarations.

`--supabase-url` enables the external Supabase logging sink during init. `--supabase-schema` defaults to `acp_stack`; `--supabase-api-key-ref` defaults to `SUPABASE_SECRET_KEY`. Interactive init prompts for a missing Supabase secret key. Non-interactive init expects `ACP_STACK_SUPABASE_SECRET_KEY` or an existing secret-store entry.

`--skills-source` and `--skills` install selected Agent Skills before testflight.
Official sources are `openai` and `anthropic`; custom sources use
`github:<owner>` and expect `<owner>/skills` on branch `main`.

Cloudflare `generated` mode writes tunnel artifacts for operator-managed setup. Cloudflare `managed` mode uses the configured secret refs to create the tunnel, push the ingress config, create or update the proxied CNAME, and emit an owner-only tunnel token env artifact during init.

## Auth And Reset

First initialization prints two API keys:

- Session key: session-driving and prompt-driving API calls.
- Admin key: secrets, config import, agent process control, and other elevated
  operations.

The plaintext values are not stored in config or `secrets.age`; local state stores only verifiers. Commands that need the session key accept `--session-key` or `ACP_STACK_SESSION_KEY`. When `[local].session_auth = "keyless"`, session-tier HTTP commands without an explicit session key use the local Unix socket instead. Commands that need the admin key accept `--admin-key`; interactive terminals prompt without echo when it is omitted.

`acps auth regenerate-session-key --admin-key <key>` rotates only the session key through the running daemon and prints the new plaintext value once. The admin key is not regenerated in place; `acps reset --yes` destroys local config, state, age key, and secret store so a new instance can be initialized.

`acps auth local-session-access status` prints the configured local session-tier mode. `enable --admin-key <key>` sets `[local].session_auth = "keyless"` through the running daemon; `disable --admin-key <key>` restores `session-key`. Both update the daemon immediately after the config write succeeds.

## Config Commands

```sh
acps config validate [path]
acps config export [--output path]
acps config export --base64
acps config import <path> [--force] [--dry-run] [--admin-key <key>]
acps config import --base64 <code> [--force] [--dry-run] [--admin-key <key>]
```

Export reads the current config file and emits canonical TOML with secret references only. Import validates and canonicalizes TOML before writing it and requires the admin key. Text output reports progress for file-writing export and import operations. Without `--force`, import refuses to replace an existing config. `--dry-run` reports what would change without writing. After a successful replace, import asks the currently configured daemon to apply `[local].session_auth`; if the daemon is unreachable, the value applies on next daemon start.

## Secret Commands

```sh
acps secrets list
acps secrets set <name> [--admin-key <key>]
acps secrets delete <name> [--admin-key <key>]
```

`secrets list` prints secret names only and does not require an auth key. `secrets set` and `secrets delete` mutate the encrypted secret store and require the admin key.

## Update Commands

```sh
acps update check
acps update install --latest [--allow-breaking]
acps update install --version <tag> [--allow-breaking]
acps update set --policy security-critical|compatible|manual [--frequency 1d]
```

`acps update` checks and installs `acp-stack` releases from `atrium-cloud/acp-stack`. Every check and install attempt writes a local update-history row and a `stack.update.*` event. Container deployments are check-only. Host installs replace `acps` only when the current binary directory is writable; systemd deployments use the root-owned updater unit installed by `scripts/install-systemd.sh`.

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

`acps agent install [--yes] [--admin-key <key>]` installs the configured supported agent from the embedded catalog. Unsupported entries fail before installation. `--yes` is accepted for scripts; install currently runs non-interactively.

`acps agent switch <agent>` migrates to another supported harness through the running daemon:

```sh
acps agent switch <agent> [--drop] [--provider <provider-id>] [--api-key-ref <ref>] [--admin-key <key>]
```

The target agent is positional. Non-interactive runs require `--admin-key`; interactive runs prompt for the admin key without echoing it. Before calling the daemon, switch prints the target install steps, config that will migrate as-is, compatible provider secret refs that will be copied if missing, optional source config cleanup, and fields that need input. Switch installs the target harness, reuses the current provider/API-key ref only when compatible, copies installed Agent Skills into the target skills directory when needed, clears the model, and prints advertised model values only when the target supports model selection. Interactive runs can select and apply a model before the command exits. Non-interactive runs print `acps agent set --model <model-id>` as the follow-up only when model selection is supported.

Switch preserves runtime-scoped config, including workspace, MCP declarations, permissions, secrets config, and sessions. By default, it also preserves source agent-owned config, secrets, and installed harnesses/adapters so switching back is fast. `--drop` removes only source agent-owned config after the target switch succeeds. It does not delete runtime MCP declarations, secrets, binaries, adapters, or sessions.

`acps agent set` updates provider, model, mode, and custom-provider metadata:

```sh
acps agent set --provider <provider-id> [--model <model>] [--api-key-ref <ref>]
acps agent set --custom-provider --provider <id> --provider-name <name> --base-url <url> --api-key-ref <ref> --model <model-id>
acps agent set --model <model>
acps agent set --mode <mode>
```

Mapped model and mode values are validated against the configured agent's ACP-advertised options. Custom-provider model ids are accepted as supplied. For provider-backed agents, `acps agent set --model <model>` uses the existing `[agent.provider]` when present. When a change requires the supervised process to reload agent-owned config, the CLI prints a restart hint.

`acps subagent *` is OpenCode-only and manages the OpenCode small-model lane. `acps subagent match` makes `small_model` follow the main agent model.

`acps agent update [--force] [--restart]` updates stale managed agent steps. By default it skips when the daemon reports an active agent process. `--restart` stops the running agent, updates, then starts it again and requires the admin key. `--restart` runs the update offline while the daemon is live, so avoid invoking it during a scheduled daemon auto-update window: both write the same install destination and have no cross-process lock.

`acps agent update set` edits the automatic update policy:

```sh
acps agent update set --auto-on
acps agent update set --auto-off
acps agent update set --frequency 3d
```

`--frequency` accepts duration suffixes such as `12h`, `1d`, `3d`, and `4w`.

`acps agent start`, `stop`, and `restart` call the running daemon with the admin key. `acps agent status` prints configured identity, process state, capability summary, and recent lifecycle information through the local read-only route. `acps agent check` reports whether managed install steps are present and current.

`acps agent test` sends a real prompt through the configured agent. It may use provider credits and should be run only when that is intentional.

## Sessions

```sh
acps sessions list [--range <day|week|month|year|all|duration>] [--range-start <datetime>] [--range-end <datetime>] [--limit <n>]
acps sessions status [--window <duration>] [--threshold <duration>] [--limit <n>]
acps sessions new [--session-key <key>]
acps sessions load <session-id> [--cwd <path>] [--session-key <key>]
acps sessions resume <session-id> [--cwd <path>] [--session-key <key>]
acps sessions fork <session-id> [--message-id <id>] [--cwd <path>] [--session-key <key>]
acps sessions prompt <session-id> [--session-key <key>]
acps sessions cancel <session-id> [--session-key <key>]
acps sessions close <session-id> [--session-key <key>]
```

`sessions list` shows the durable local session list after any supported ACP session-list sync. Sessions discovered from the agent but not loaded locally are shown as `available`. `sessions list` and `sessions status` use the local read-only socket and do not require a session key.

`sessions status` prints sessions with activity in a rolling window and a derived turn state such as `prompt_sent`, `working`, `permission_required`, `done`, or `error`. The default window is `8h`; `--window` accepts `1m` through `999h`. `--threshold` remains as the recency threshold for the `recent` field.

Session CWD values must be existing absolute directories that canonicalize under `[workspace].root`; stored CWD defaults are rechecked before load, resume, or fork.

`sessions new`, `load`, `resume`, `fork`, `prompt`, `cancel`, and `close` affect inference session state and require `--session-key` or `ACP_STACK_SESSION_KEY` unless `[local].session_auth = "keyless"` is active. `sessions load` and `sessions resume` call the matching ACP session operation through the daemon. `sessions fork` creates a child session through ACP. `--message-id` forks from an acknowledged prompt message id when the agent advertises that capability. `sessions close` closes the agent-side session and preserves local history; permanent deletion is deferred until product semantics are defined.

## Logs, Metrics, And Health

`acps status` validates local config and state, prints workspace and agent status, and probes daemon readiness through the local socket when the daemon is reachable.

`acps logs query` reads durable events without a session key. Filters include level, kind or kind prefix, source, session id, command id, permission id, security category, time bounds, and cursor. `--order <asc|desc>` flips sort direction (default `desc`). `--json` emits the `{ events, next_cursor }` envelope to stdout and suppresses the human "more rows" hint. `--category <rate_limit|origin_cors|ip_block|oversized_request>` scopes to one security category. `--follow` subscribes to the daemon's `logs` WebSocket topic, drains matching durable backlog in ascending pages, then continues with live events and requires the session key. With `--json --follow`, stdout is newline-delimited `EventJson` objects rather than the non-follow envelope.

`acps logs tail` opens a WebSocket subscription to the running daemon and requires the session key.

`acps metrics summary` prints the daemon's summary metrics for a time window through the local read-only route.

`acps ws connections` and `acps ws sessions` use the local read-only route. `acps ws disconnect` mutates live public WebSocket state and requires the admin key.

`acps security check` runs the security self-check through the local diagnostic route and persists the run to history. `acps security history [--limit N] [--after <id>] [--json]` lists prior runs newest-first; `acps security show <run-id> [--json]` prints a single recorded run with its findings. Security history/show require the admin key. `acps deps check` reports declared dependency status from local config. `acps deps apply` runs only install actions declared in config, requires the admin key, and requires confirmation unless `--yes` is passed. Apply output includes the durable `apply_run_id`; failed runs point operators to `acps installer history --agent deps_apply`.

## Shell Completion

`acps completion <bash|zsh|fish|powershell|elvish>` writes a completion script to stdout.
