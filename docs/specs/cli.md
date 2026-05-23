# CLI

`acps` is the local command-line interface for initializing, running, inspecting, and operating an `acp-stack` instance.

The CLI should call the same core service layer as the HTTP API where practical. It should not grow a separate behavior path that diverges from the daemon.

## Commands

Initial CLI commands:

```sh
acps init [--agent <id>] [--provider <provider-id>] [--api-key-ref <ref>] [--custom-provider ...] [--workspace-root <path>] [--workspace-uploads <path>] [--runtime-user <name>] [--code-from <url>]... [--data-from <path-or-url>]... [--skip-workspace-init] [--testflight|--skip-testflight]
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
acps config import --dry-run <path>
acps config import --dry-run --base64 <code>

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

`acps init` creates or validates local config and state, initializes the age-encrypted secret store, and generates the two API keys named by `[auth]`. Interactive init prompts for one supported agent, updates `[agent]` with the registry-recommended launch command, then asks whether to install that agent. Non-interactive init skips agent selection and install unless `--agent <id>` and/or `--install-agent` are supplied; `--no-install-agent` suppresses the install prompt in interactive runs. Provider-backed init fails before writing provider config unless every required secret ref exists; interactive init may collect missing values, while non-interactive init requires the refs to already be present.

When creating a starter config, deployment tooling may pass `--workspace-root`, `--workspace-uploads`, and `--runtime-user` so the generated `[workspace]` block matches the process manager's user, working directory, and writable paths. These flags affect only a newly-created config; re-running init against an existing config validates and preserves the file, and rejects deployment override values that contradict the persisted `[workspace]` block.

`acps init --edge cloudflare --exposure tunnel --hostname <host>` is the recommended public deployment profile. It keeps `acps` bound to `127.0.0.1`, sets `[api].public_url` and explicit `allowed_origins` to the Cloudflare hostname, trusts only local `cloudflared` proxy peers, adds a host `cloudflared` dependency when applicable, and emits local `cloudflared` config plus systemd/Docker snippets. Managed Cloudflare API provisioning is deferred.

`acps init` seeds the workspace from any number of declared
`[[workspace.code_sources]]` and `[[workspace.data_sources]]` entries
(see [config.md](config.md#workspace-source)). For ad-hoc initial setup,
`--code-from <repo-url>` (repeatable) appends a `type = "git"`
code-source and `--data-from <path-or-url>` (repeatable) appends a
`type = "local"` or `type = "https"` data-source to the *starter*
config. `--data-from http://...` is rejected at parse time; only
absolute local paths and `https://` URLs are accepted. Both flags
affect only a newly-created starter config; on a re-run with an
existing config they are ignored in favor of the persisted source
list.

`--skip-workspace-init` bypasses the materializer entirely. Useful for
test loops and for operators who want to apply workspace sources
manually after init completes.

Phase 4 expands init into a resumable orchestration flow:

- create or import config
- prompt for the agent, provider id, and missing required secret references without echoing values
- resolve provider selection through the provider/env mapping and validate model/mode values through ACP session config options
- update generated OpenCode or Pi provider config and relaunch the active agent when provider/model settings change
- set up code sources under `/workspace/usr/code/<repo-name>/`
- set up data sources under `/workspace/usr/data/<data-dir-name>/`
- run agent harness and adapter installation as runtime-managed installer steps
- configure declared MCP servers and presets
- run a real-prompt agent testflight after explicit confirmation

Dependency installation requires explicit install metadata and remains a separate future `deps apply`/`data/deps.toml` surface. `acps init` does not infer OS package-manager actions from declarative dependency checks.

For supported OpenCode and Pi configs, `acps init` does not infer model config from default API-key refs. Init may select the initial provider, collect the required provider refs, and write `[agent.provider]` without a model. `acps agent set` is the edit path that can later write the model.

When a selected provider has no default env mapping and the agent supports custom providers, interactive init can collect custom provider fields and write generated agent config. Non-interactive init requires the explicit custom flags: `--custom-provider --provider <id> --provider-name <display-name> --base-url <url> --api-key-ref <ref> --model <model-id>`, with optional `--provider-api <chat-completions|responses>`, `--model-name <display-name>`, `--context <tokens>`, and `--output-max-tokens <tokens>`.

## Serve

`acps serve` runs the HTTP daemon in the foreground. It blocks the calling shell until it receives `SIGTERM` or `SIGINT`, at which point it triggers a graceful shutdown and exits. The expected deployment is to run it under a process manager (`systemd`, `launchd`, supervisord, a container init) — `acps` itself does not daemonize, does not write a PID file, and does not fork. Standard error carries the startup and shutdown announcement; structured runtime history goes to the SQLite `agent_lifecycle` table as `server.starting`, `server.started`, and `server.stopped` rows.

Bind defaults to `[api].bind` from config (`127.0.0.1:7700`). `--bind <addr>` overrides it for this run. The HTTP server enforces the request body cap as `min([api].max_request_bytes, [security.http].max_request_bytes)` and 413s oversized requests before any handler runs.

`acps serve` requires both API keys to already exist in the encrypted secret store under the names declared in `[auth]`; missing keys fail startup before the listener binds. The daemon refuses to run as root unless `--allow-root` is passed or `ACP_STACK_ALLOW_ROOT=1` is set; the environment opt-in is exact, so an empty value or `0` is ignored.

## Agent Commands

`acps agent install` installs the configured ACP agent process. The operator-facing path resolves the agent from the embedded registry, refuses unsupported entries, runs the declared harness step, runs the adapter step concurrently for adapter-backed entries, records every installer row in SQLite, verifies `creates`, and checks `expected_sha256` against the final `[agent].command` binary when configured. Registry-resolved installs do not require or receive `[agent].env` runtime secrets. Direct shell recipes are a low-level/manual escape hatch, not the preferred discovery or installation path.

`acps agent set --provider <provider-id> [--model <model>] [--api-key-ref <ref>]` updates `[agent.provider]`, adds the selected API-key ref and provider companion refs to `[agent].env` when missing, validates the selected model against ACP-advertised model options, regenerates supported agent-owned config files, and then writes canonical config. Provider edits are available only for agents whose registry entry declares `set_provider = true`. When `--api-key-ref` is omitted, the CLI resolves the default ref from the provider mapping in [agents/api_key.md](agents/api_key.md); providers without a default require custom provider setup. Codex `openai` is the exception: it requires `--model`, rejects `--api-key-ref`, and keeps auth Codex-native. When provider-backed `--model` is omitted, interactive terminals prompt from the ACP-advertised model list, while non-interactive runs list advertised model values and exit without mutating config.

`acps agent set --custom-provider --provider <id> --provider-name <display-name> --base-url <url> --api-key-ref <ref> --model <model-id>` writes `[agent.provider.custom]`, adds the API-key ref to `[agent].env`, skips ACP model-list validation for the custom model id, regenerates the supported agent-owned config, and writes canonical config only after provisioning succeeds. Optional flags are `--provider-api <chat-completions|responses>`, `--model-name <display-name>`, `--context <tokens>`, and `--output-max-tokens <tokens>`. Token limits are plain positive integers with no commas. OpenCode, Pi, and Goose default custom providers to `chat-completions`; Codex defaults to `responses` and rejects `chat-completions`. Amp Code and Cursor CLI reject custom provider/model setup.

`acps agent set --model <model>` updates `[agent].model` for model-only agents whose registry entry declares `set_model = true` and `set_provider = false`. Cursor CLI uses this path; `acps` validates the value against Cursor's ACP-advertised model options and stores the exact advertised value.

`acps agent set --mode <mode>` updates `[agent].mode` for agents whose registry entry declares `set_mode = true` and whose ACP session advertises a `mode` config option. Current real ACP probes advertise OpenCode `build`/`plan`, Cursor `agent`/`ask`/`plan`, Codex `read-only`/`auto`/`full-access`, and `amp-acp v0.1.1` `smart`/`rush`/`deep`; Pi and Goose do not advertise mode values.

Successful `acps agent set` output prints the configured agent id, changed fields, and `settings will take effect on new sessions`.

`acps agent start` and `acps agent stop` call the running daemon over HTTP using the admin key from the encrypted secret store. The base URL is `[api].public_url` when configured; otherwise it is derived from `[api].bind`, with wildcard binds rewritten to loopback for local CLI calls.

`acps agent test [--prompt <text>]` starts the configured ACP agent directly, runs `initialize`, creates a session, sends a real prompt, requires `end_turn` prompt completion, and shuts the agent down before exiting. When `--prompt` is omitted, the CLI uses the active registry entry's `testflight_prompt` when present, otherwise a built-in minimal compatibility prompt. Registry entries may also declare `testflight_expect_fs`; in that case the test removes a stale regular marker before the prompt and then requires a non-empty regular file under `workspace.root` after prompt completion. Failures identify the first failing stage: spawn/start, ACP initialize, session creation, prompt/progress timeout, prompt completion, shutdown, or filesystem smoke.

`acps agent status` reads local config, the active agent registry, and SQLite state. It prints `agent: <id>`, configured agent params as individual `provider:`, `model:`, and `mode:` lines, grouped supported-but-unconfigured params as `<params> unset`, grouped unsupported params as `<params> unavailable`, the latest successful agent-scoped installer versions as `installed <step>: <version>` or `installed <step>: version unknown`, then the configured command, latest persisted capability snapshot, and recent lifecycle rows. Legacy installer rows without `agent_id` are not shown because they cannot be safely attributed to the active agent.

## Security Self-Check

`acps security check` runs the local self-check described in [security](security.md). Findings render in the rule order produced by `security::check()` as `- <severity> <code>: <message>`; when a finding carries a remediation hint it appears on an indented `hint:` line directly below the diagnostic:

```text
findings:
- critical runtime.path_mode_loose: config directory at /home/acp/.config/acp-stack has mode 0o755, expected 0o700
    hint: Run `chmod 0700 -- '/home/acp/.config/acp-stack'` to restore owner-only permissions.
```

## Local Agent Interface

`acpctl` is separate from `acps`. It is the constrained local, agent-facing interface described in [acpctl](acpctl/acpctl.md).

## Current Implementation Subset

The first implemented CLI surface focuses on local config, durable state, the secret store, and the foreground HTTP daemon. `init`, `status`, and `logs query` all create or migrate the local SQLite file when missing:

- `acps --version`
- `acps config validate [path]`
- `acps config export [--output path]`
- `acps config export --base64`
- `acps config import <path> [--force] [--dry-run]`
- `acps config import --base64 <code> [--force] [--dry-run]`
- `acps init`
- `acps init --workspace-root <path> --workspace-uploads <path> --runtime-user <name>`
- `acps init --edge cloudflare --exposure tunnel --hostname <host>`
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
- `acps security check`
- `acps ws connections`
- `acps ws sessions`
- `acps ws disconnect --connection-id <id>`
- `acps ws disconnect --session-id <id>`
- `acps serve [--bind <addr>] [--allow-root]`

When `[path]` is omitted for validation, the CLI reads `~/.config/acp-stack/acp-stack.toml`. Export currently reads the same default path and writes canonical TOML to stdout unless `--output` is provided.

`acps init` creates the default config and state directories, writes a valid starter config when one is absent, validates an existing config without overwriting it, creates or migrates `~/.local/share/acp-stack/state.sqlite`, initializes the age key and the encrypted secret store, generates session and admin API keys when the store is fresh, and records `init.completed` and `auth.keys_generated` events. On a re-run with both API keys already present, init preserves them silently; if either reference name is missing in a non-empty store, init fails fast.

`acps config import` validates the incoming TOML and writes it to the default config path as canonical TOML. By default, import refuses to overwrite an existing config; pass `--force` to replace one. `--base64 <code>` decodes its argument as base64-encoded canonical TOML before validation. `--dry-run` validates, canonicalizes, compares auth refs, and reports metadata without writing to disk or auditing. Import input size (raw TOML or decoded base64) is capped at 1 MiB.

`acps secrets set <name>` reads a single line from stdin and stores it as the named secret. `acps secrets list` prints names only — values are never echoed. `acps secrets delete <name>` removes the named secret and errors when it does not exist.

`acps status` validates the default config, opens or migrates local state, records `status.checked`, and prints config, state, schema version, and latest event status.

`acps logs query` reads durable SQLite events newest-first. `--limit` defaults to `50`. Additional filters: `--level <level>` (exact match); `--kind <kind>` (exact, or dotted prefix when the value ends with `.`); `--source <writer>` (`api`/`acp`/`command`/`permission`/`cli`/`system`); `--session <id>`, `--command <id>`, `--permission <id>` for cross-reference lookups; `--since` and `--until` accept either an RFC3339 timestamp or a duration suffix (`30m`, `1h`, `2d`, `1w` — interpreted as "this much time ago"); `--after <event-id>` continues a keyset-paginated scan past the previous page's last row. Each output line is `<created_at> <level> <source> <kind> <message>`.

`acps metrics summary` calls `/v1/metrics/summary` on the running daemon and pretty-prints the JSON response. Without `--since` the window defaults to 24h; the same duration/RFC3339 form as `logs query` is accepted.

`acps logs tail` opens a WebSocket subscription to the running daemon and prints each frame as it arrives until SIGINT. `--topic <name>` may be repeated to subscribe to multiple topics; the default is `logs`. Authentication uses the session key from the encrypted secret store, so the daemon must be reachable at `[api].public_url` (or the loopback rewrite of `[api].bind`).

WebSocket connection management belongs under `acps ws`:

```sh
acps ws connections
acps ws sessions
acps ws disconnect --connection-id <connection-id>
acps ws disconnect --session-id <session-id>
```

`connections` and `sessions` report live `/v1/ws` clients and unique subscribed `sessions.{id}` topics. `disconnect` requires the admin key and closes only WebSocket client sockets; it does not close ACP sessions or cancel prompts.
