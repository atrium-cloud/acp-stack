# CLI Flag Reference

Reference for `acps` commands and flags. Auth tiering, output-format contracts, restart levels, and the `acps agent test` JSON contract are explained in [cli.md](cli.md).

Commands read `~/.config/acp-stack/acps-config.toml` by default unless an explicit path argument is documented.

## Global Flags

- `--format text|json`: output format for most operator commands. Text is the default.
- Text-only commands reject `--format json` instead of silently ignoring it.
- `--json`: accepted as an alias for `--format json` on commands that already carry it. Conflicts with an explicit `--format text`.

## `acps init`

Creates or validates config and state, initializes the encrypted secret store, and generates API keys on first run. It can also optionally configure an agent, provider, workspace sources, MCP servers, edge profile, and testflight. The interactive flow and step sequence are in [init.md](../init.md).

### Synopsis

```sh
acps init \
  [--from-file <path>|--from-toml <toml>|--from-base64 <base64>] \
  [--agent <id>] [--non-interactive] \
  [--custom-agent-id <id> --custom-agent-command <cmd> --custom-agent-install <shell> \
   [--custom-agent-name <name>] [--custom-agent-arg <arg>]... [--custom-agent-creates <path>]] \
  [--adapter-override-command <cmd> (--adapter-override-install-npm <package>|--adapter-override-install-shell <shell>) \
   [--adapter-override-arg <arg>]... [--adapter-override-github <repo>] [--adapter-override-install-creates <path>] \
   | --adapter-override-clear] \
  [--agent-env-ref <name>]... \
  [--dep <name=shell>]... [--dep-system <name=shell>]... [--deps-apply [--deps-apply-yes] [--deps-apply-async]] \
  [--stack-update <on|security|off> [--stack-update-frequency <freq>]] \
  [--agent-update <on|off> [--agent-update-frequency <freq>]] \
  [--skills-source <source|github:owner>] [--skills <selector,selector>] [--no-skills] \
  [--provider <provider-id>] [--api-key-ref <ref>] [--model <model-id>] [--mode <mode-id>] [--effort <effort-id>] \
  [--custom-provider --provider <id> --provider-name <name> --base-url <url> --api-key-ref <ref> --model <model-id>] \
  [--workspace-root <path>] [--workspace-uploads <path>] [--runtime-user <name>] \
  [--sandbox <off|unshare|bwrap|custom>] \
  [--code-from <repo-url>]... [--data-from <path-or-url>]... \
  [--mcp-preset linear] [--mcp-stdio <name=command>]... [--mcp-stdio-env <server=SECRET_REF>]... \
  [--mcp-http <name=https://...>]... [--mcp-http-header <server=Header:SECRET_REF>]... \
  [--supabase-url <url>] [--supabase-schema <schema>] [--supabase-api-key-ref <ref>] [--no-supabase] \
  [--edge cloudflare --exposure tunnel --hostname <host>] [--cloudflare-mode generated|managed] \
  [--cloudflare-api-token-ref <ref> --cloudflare-account-id-ref <ref>] \
  [--testflight|--skip-testflight] [--resume [--run-id <id>] | --fresh] \
  [--rotate-keys] [--handoff-json]
```

### Flags

#### Config source

- `--from-file <path>`, `--from-toml <toml>`, `--from-base64 <base64>`: initialize from an existing `acps-config.toml`.
- `--from-toml` takes raw TOML for scripted input. `--from-base64` takes the same TOML base64-encoded for safer terminal paste.
- The interactive source prompt offers file import and base64 paste.

#### Agent selection

- `--agent <id>`: selects a registry agent.
- `--non-interactive`: skips all prompts; every value must come from flags.
- A non-interactive first run requires `--agent <id>`, the `--custom-agent-*` set, or a complete imported config.
- `--custom-agent-id <id>` with `--custom-agent-command <cmd>` and `--custom-agent-install <shell>`: declare a custom (non-registry) agent and write an `[agent.install]` shell escape hatch.
- Optional custom-agent flags: `--custom-agent-name <name>`, repeatable `--custom-agent-arg <arg>`, `--custom-agent-creates <path>`.
- The custom-agent set is also offered as a "Custom agent" choice in the interactive picker.
- The custom-agent set conflicts with the `--provider`/`--model`/`--mode`/`--effort` init flags. Custom agents configure those through their own environment.
- `--adapter-override-command <cmd>` plus exactly one of `--adapter-override-install-npm <package>` or `--adapter-override-install-shell <shell>`: declare a designated ACP adapter for a registry agent, writing `[agent.adapter_override]` (see [config.md](../config.md)).
- The adapter override applies to the `--agent` selection, or to the already-configured registry agent when `--agent` is omitted. It conflicts with the `--custom-agent-*` set.
- Optional adapter-override flags: repeatable `--adapter-override-arg <arg>`, `--adapter-override-github <repo>`, `--adapter-override-install-creates <path>`.
- `--adapter-override-clear`: removes the override.
- The github-release adapter install variant has no flag form; it is declared in an imported config.
- `--agent-env-ref <name>` (repeatable): adds secret-backed environment variables to `[agent].env`. New config only. The named secret must already resolve in the store. Interactive runs can collect masked values when Agent environment is selected.

#### Dependencies

- `--dep <name=shell>` (repeatable, user scope), `--dep-system <name=shell>` (repeatable, system scope): declare `[dependencies.commands]` install actions. New config only.
- `--deps-apply`: runs the pending install actions during init. It confirms interactively; non-interactive runs additionally require `--deps-apply-yes`.
- `--deps-apply-async`: runs the confirmed install in a detached worker so init can continue.
- Apply outcomes are recorded under `acps installer history --agent deps_apply`.
- System-scope actions run directly as root, or through `sudo -n` when passwordless sudo is available. Otherwise they are skipped as `privilege_required` with the manual `sudo` commands printed, and init continues.

#### Update policies

- `--stack-update <on|security|off>`: sets the `[updates.acp_stack]` policy (`on` = all compatible, `security` = security-critical only, `off` = manual).
- `--stack-update-frequency <freq>`: sets the schedule at day/week granularity (minimum `1d`, e.g. `1d`, `3w`) for non-off policies.
- A non-interactive run that omits both keeps the defaults (security-critical, `1d`).
- `--agent-update <on|off>`: sets `[agent.auto_update]`. `on` auto-updates the managed agent's harness/adapter while it is stopped; `off` leaves updates manual through `acps agent update`.
- `--agent-update-frequency <freq>`: sets the schedule at day/week granularity (minimum `1d`) when on.
- A non-interactive run that omits both keeps the managed-agent default (`on`, `1d`). Custom (non-registry) agents cannot auto-update, so `--agent-update on` is rejected for them.

#### Skills

- `--skills-source <source|github:owner>`, `--skills <selector,selector>`: install selected Agent Skills before testflight. `--no-skills` skips skills.
- Reviewed source aliases are defined in [data/skills.toml](../../../data/skills.toml). Custom sources use `github:<owner>` and expect `<owner>/skills` on branch `main`.
- A selector is usually a skill name. When a source contains distinct same-name variants, the catalog exposes a path-qualified selector.

#### Provider, model, mode, effort

- `--provider <provider-id>`, `--api-key-ref <ref>`, `--model <model-id>`: select provider, credential ref, and model. Where `--model` skips advertisement validation (a custom provider, or a harness that reads the model from its own config), the value is trimmed, rejected when empty, and otherwise written as given.
- `--mode <mode-id>`, `--effort <effort-id>`: set a session mode or reasoning effort non-interactively. Both are validated against the agent's ACP-advertised values. Codex with OpenRouter validates `--effort` against the provider catalog's reasoning-effort values for the configured model, since the adapter advertises none for OpenRouter models. Goose resolves its model while starting a session, so both flags require a model — pass `--model` alongside them or configure one first.
- The mode lane applies only to agents that support session modes. The effort lane applies only to agents that advertise a reasoning-effort (`thought_level`) config option.
- A non-interactive run without `--mode`/`--effort` writes no mode or effort.
- `--custom-provider --provider <id> --provider-name <name> --base-url <url> --api-key-ref <ref> --model <model-id>`: declare a custom provider.
- Discovery and validation behavior: [init.md](../init.md#flow).

#### Workspace

- `--workspace-root <path>`, `--workspace-uploads <path>`, `--runtime-user <name>`, `--sandbox <off|unshare|bwrap|custom>`: affect only a new starter config. Once config exists, supported post-init workspace changes go through `acps workspace *`.
- `--sandbox` sets `[workspace.sandbox].mode` (default `off`) and applies only when the starter config is being created. `custom` additionally requires a `wrapper`, supplied through an imported config. See [security.md](../security.md#sandbox).
- `--code-from <repo-url>` (repeatable): appends Git code sources to a new starter config.
- `--data-from <path-or-url>` (repeatable): appends local or HTTPS data sources to a new starter config only. Against an existing config it is rejected, and `[[workspace.data_sources]]` is edited directly instead. Plain HTTP URLs are rejected. Interactive init can also collect S3 data sources.

#### MCP

- `--mcp-preset linear`: adds the Linear hosted MCP declaration using `LINEAR_API_KEY`. New starter configs only.
- `--mcp-stdio <name=command>`, `--mcp-http <name=https://...>` (repeatable): add custom runtime-wide MCP declarations. HTTP URLs must be https, or http to a loopback host (a local relay endpoint).
- `--mcp-stdio-env <server=SECRET_REF>` (or `server=VAR=template`), `--mcp-http-header <server=Header:SECRET_REF>` (or `server=Header:=template>`): attach secret refs or `${SECRET_REF}`-interpolated template values to those declarations. Template rules: [config.md](../config.md#secret-reference-templates).
- On runs that create a config (or `--resume`), interactive init offers masked entry for declared secret refs missing from the store. This covers MCP and S3 data-source refs, including refs named inside templates. Skipping one leaves it to resolve later: MCP health for MCP refs, workspace materialization for S3 refs.

#### Supabase

- `--supabase-url <url>`: enables the external Supabase logging sink during init. `--no-supabase` declines it.
- `--supabase-schema <schema>` defaults to `acp_stack`. `--supabase-api-key-ref <ref>` defaults to `SUPABASE_SECRET_KEY`.
- Interactive init prompts for a missing Supabase secret key. Non-interactive init expects `ACP_STACK_SUPABASE_SECRET_KEY` or an existing secret-store entry.

#### Edge

- `--edge cloudflare --exposure tunnel --hostname <host>` with `--cloudflare-mode generated|managed`: configure a Cloudflare edge profile.
- `generated` mode writes tunnel artifacts for operator-managed setup.
- `managed` mode uses `--cloudflare-api-token-ref <ref>` and `--cloudflare-account-id-ref <ref>`. It creates the tunnel, pushes the ingress config, creates or updates the proxied CNAME, and emits an owner-only tunnel token env artifact during init.

#### Run control

- `--testflight`, `--skip-testflight`: run or skip the post-init testflight.
- `--resume [--run-id <id>]`, `--fresh`: continue an unfinished run or force a new one. Semantics: [init.md](../init.md#the-resumable-run).
- `--rotate-keys`: regenerates the session and admin keys in place over existing verifier rows and prints the new plaintexts once. A running daemon must be restarted to accept them.
- `--handoff-json`: disables prompts and emits only the handoff JSON object described in [init.md](../init.md#platform-handoff-json). `acps init --format json` remains rejected; `--handoff-json` is the scripted form for this narrower contract.

### Output

- Init creates or validates the workspace root and uploads directory, then installs the configured real agent. Adapter-backed agents install both harness and adapter unless the catalog marks the harness as adapter-provided.
- Re-running init preserves existing API keys and config unless an explicit option requests a fresh run.
- The run summary and one-time key handover: [init.md](../init.md#key-handover).

## `acps init serve`

Starts the hosted bootstrap HTTP/WebSocket API documented in [api.md](../api/api.md#bootstrap-init-api).

### Synopsis

```sh
acps init serve [--token-env <var>] [--token-file <path>] [--idle-timeout <duration>] [--max-lifetime <duration>]
```

### Flags

- Bootstrap token: from `ACP_STACK_INIT_TOKEN`, `--token-env`, or `--token-file`. The token is process-local and is not written to config or state.
- `--idle-timeout` (default `15m`; `0s` disables): cancel the session once there has been no connected WebSocket client and no API activity for that long.
- `--max-lifetime` (disabled by default): cap the absolute server lifetime regardless of activity.
- Both durations accept `s/m/h/d/w` suffixes.

### Output

- An expired session is reported as `cancelled` and the process exits non-zero. Reaching either limit before any session was created also exits non-zero.
- Attached WebSockets are closed server-side once the session turns terminal, so a hung client cannot hold the process past `--max-lifetime`.
- An init session that fails before key handover parks as `errored` until the backend acknowledges the error (`ack_error`) or a 2-minute grace expires; the server then exits non-zero. The grace check runs even with `--idle-timeout 0s`.

## `acps auth regenerate-session-key`

Rotates only the session key through the running daemon.

### Synopsis

```sh
acps auth regenerate-session-key --admin-key <key>
```

### Flags

- `--admin-key <key>`: required.

### Output

- Prints the new plaintext session key once.

## `acps auth local-session-access`

Manages the local session-tier auth mode.

### Synopsis

```sh
acps auth local-session-access status
acps auth local-session-access enable --admin-key <key>
acps auth local-session-access disable --admin-key <key>
```

### Flags

- `--admin-key <key>`: required for `enable` and `disable`.

### Output

- `status` prints the configured local session-tier mode.
- `enable` sets `[local].session_auth = "keyless"` through the running daemon; `disable` restores `session-key`. Both update the daemon immediately after the config write succeeds.

## `acps reset`

Destroys the local instance so a new one can be initialized.

### Synopsis

```sh
acps reset --yes
```

### Flags

- `--yes`: confirm the destructive reset.

### Output

- Deletes local config, state, age key, and secret store.

## `acps config`

Validates, exports, and imports the canonical config file.

### Synopsis

```sh
acps config validate [path]
acps config export [--output path]
acps config export --base64
acps config import <path> [--force] [--dry-run] [--admin-key <key>]
acps config import --base64 <code> [--force] [--dry-run] [--admin-key <key>]
```

### Flags

- `--output path`: write the export to a file instead of stdout.
- `--base64`: emit (export) or accept (import) the config as base64-encoded TOML.
- `--force`: allow import to replace an existing config. Without it, import refuses the replace.
- `--dry-run`: report what would change without writing.
- `--admin-key <key>`: required for import.

### Output

- Export reads the current config file and emits canonical TOML with secret references only.
- Import validates and canonicalizes TOML before writing it.
- Text output reports progress for file-writing export and import operations.
- After a successful replace, import asks the currently configured daemon to apply `[local].session_auth`. If the daemon is unreachable or rejects the local admin key, the value applies on next daemon start.

## `acps workspace`

Manages workspace paths, sources, sync, and the sandbox mode.

### Synopsis

```sh
acps workspace status
acps workspace code-source list
acps workspace code-source add --repo <repo> [--branch <branch>] [--credential-ref <ref>] [--name <name>] [--no-sync]
acps workspace data-source list
acps workspace data-source add --type <local|https|s3> [source flags] [--name <name>] [--no-sync]
acps workspace sync
acps workspace sandbox status
acps workspace sandbox set --mode <off|unshare|bwrap|custom> [--wrapper-arg <arg>]...
```

### Flags

- `--repo <repo>`, `--branch <branch>`, `--credential-ref <ref>`, `--name <name>`: code-source fields.
- `--type <local|https|s3>` plus per-type source flags: data-source fields.
- `--no-sync`: write config only; skip the default `workspace sync` run.
- `--mode <off|unshare|bwrap|custom>`, repeatable `--wrapper-arg <arg>`: sandbox fields.

### Output

- `workspace status` prints configured workspace paths, source counts, and sandbox mode.
- `code-source add` appends a Git source under `workspace.root/usr/code`. `data-source add` appends a local, HTTPS, or S3 source under `workspace.root/usr/data`.
- Source additions validate and write canonical config, then run `workspace sync` by default.
- `workspace sync` creates missing workspace base directories and syncs every configured source. Existing source destinations with matching sentinels are verified and skipped.
- `workspace sandbox set` manages only `[workspace.sandbox].mode` and `wrapper`; existing extra mask and allow paths and any declared extensions are preserved.
- Extension declarations have no CLI flags; they are configured only through imported or directly edited TOML. A mode change that would conflict with a declared network-provider extension (anything other than `unshare`) fails without writing. The error points at the extension to remove or change first.
- Non-`off` modes are preflighted before config is written.
- `workspace sandbox status` additionally reports the network isolation state, provider, timeout, and provider stderr routing when a network-provider extension is declared.
- Sandbox changes require a supervised-agent restart with `acps restart`; they do not require a daemon restart.

## `acps extensions status`

Lists the declared `[extensions]` instances read-only.

### Synopsis

```sh
acps extensions status
```

### Flags

- No command-specific flags. There is no mutating extensions CLI; declarations are edited in the config TOML.

### Output

- For a network-provider instance: the type and provider settings.
- For a managed-state instance: the capability, applied revision, and provider id (never values).
- Managed-state namespaces are written only by their external orchestrator through the admin apply endpoint. See [extensions.md](../extensions.md).

## `acps secrets`

Manages the encrypted secret store.

### Synopsis

```sh
acps secrets list
acps secrets set [<name>] [--name <name>] [--value <value>] [--admin-key <key>]
acps secrets delete <name> [--admin-key <key>]
```

### Flags

- `--name <name>`: secret name (alternative to the positional argument).
- `--value <value>`: secret value. When omitted, interactive runs prompt without echo and non-interactive runs read one line from stdin.
- `--admin-key <key>`: required for `set` and `delete`.

### Output

- `secrets list` prints secret names only and does not require an auth key.
- The prompt or stdin form avoids shell history and process-argument exposure.

## `acps update`

Checks and installs `acp-stack` releases from `atrium-cloud/acp-stack`.

### Synopsis

```sh
acps update check
acps update install --latest [--allow-breaking]
acps update install --version <tag> [--allow-breaking]
acps update set --policy security-critical|compatible|manual [--frequency 1d]
```

### Flags

- `--latest`, `--version <tag>`: select the release to install.
- `--allow-breaking`: permit a breaking-release install.
- `--policy security-critical|compatible|manual`, `--frequency 1d`: set the update policy and schedule.

### Output

- Every check and install attempt writes a local update-history row and a `stack.update.*` event.
- Container deployments are check-only.
- Host installs replace `acps` only when the current binary directory is writable. systemd deployments use the root-owned updater unit installed by `scripts/install-systemd.sh`.

## `acps logging supabase`

Manages the external Supabase logging sink.

### Synopsis

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

### Flags

- `--url <url>`: Supabase project URL.
- `--project-ref <ref>`, `--yes`: setup options.
- `--schema <schema>`, `--api-key-ref <ref>`, `--db-url-ref <ref>`: sink configuration refs.

### Output

- `setup` uses the Supabase CLI to provision table-backed logging, then stores only the narrow runtime writer DB URL in the encrypted secret store.
- `check` writes a marked canary row to prove the configured backend can receive logs.
- `set-secret` remains for the legacy PostgREST backend.
- Status output reports whether configured secrets exist but never prints their values.

## `acps agent install`

Installs the configured supported agent from the embedded catalog.

### Synopsis

```sh
acps agent install [--yes] [--admin-key <key>]
```

### Flags

- `--yes`: accepted for scripts; install currently runs non-interactively.
- `--admin-key <key>`: admin auth.

### Output

- Unsupported catalog entries fail before installation.

## `acps agent switch`

Migrates to another supported harness through the running daemon.

### Synopsis

```sh
acps agent switch <agent> [--drop] [--provider <provider-id>] [--api-key-ref <ref>] [--admin-key <key>]
```

### Flags

- `<agent>`: target agent, positional.
- `--drop`: remove only the source agent-owned config after the target switch succeeds. Does not delete runtime MCP declarations, secrets, binaries, adapters, or sessions.
- `--provider <provider-id>`, `--api-key-ref <ref>`: provider selection for the target.
- `--admin-key <key>`: required for non-interactive runs; interactive runs prompt without echoing it.

### Output

- Before calling the daemon, switch prints the target install steps, config that will migrate as-is, compatible provider credentials, optional source config cleanup, and fields that need input.
- Switch installs the target harness and reuses a compatible flat ref or the current structured provider/alias selection.
- Installed Agent Skills are copied into the target skills directory when needed. Same-named target skills without the managed marker are left untouched and printed as kept unmanaged. Symlinks are refreshed when the target declares a separate discovery directory, e.g. Claude Code's `~/.claude/skills`. The marker and link rules: [skills.md](../agents/skills.md).
- Switch clears the model and prints advertised model values only when the target supports model selection. Interactive runs can select and apply a model before the command exits. Non-interactive runs print `acps agent set --model <model-id>` as the follow-up only when model selection is supported.
- Switch preserves runtime-scoped config: workspace, MCP declarations, permissions, secrets config, and sessions. By default it also preserves source agent-owned config, secrets, and installed harnesses/adapters, so switching back is fast.
- A switch is journaled in `agent-switch.json` beside the canonical config, so a failure after the config write — e.g. the new agent's first start — does not strand the daemon.
- Retrying the same target resumes the interrupted switch and converges it (`provider_status: "resumed"`). Retrying a finished switch is a no-op success (`provider_status: "no_op"`). Requesting a different target while a switch is incomplete fails with `409 agent.switch_conflict`.

## `acps agent provider`

Manages mapped providers and their encrypted credential catalog.

### Synopsis

```sh
acps agent provider use <provider-id> [--model <model>]
acps agent provider set-active <provider-id,provider-id,...>
acps agent provider list-active
acps agent provider credential add <provider-id>
acps agent provider credential update <provider-id> [alias]
acps agent provider credential select <provider-id> <alias>
acps agent provider credential list [provider-id]
acps agent provider credential delete <provider-id> [alias]
```

### Flags

- `--model <model>`: model to pair with `use`.
- `--from-secret ENV=REF` (repeatable): copy encrypted values into new credentials from scripts.
- `[alias]`: credential alias; the first credential is aliasless.

### Output

- Adding a second credential prompts for names for both keys and preserves every affected target's existing selection.
- `set-active` is limited to OpenCode and Pi. It does not change the default provider or selected aliases.
- `list-active` reports the environment acps configured and the last successfully loaded snapshot. It marks loaded state unknown when the daemon is unavailable.

## `acps agent set`

Updates model, mode, effort, and custom-provider metadata.

### Synopsis

```sh
acps agent set --custom-provider --provider <id> --provider-name <name> --base-url <url> --api-key-ref <ref> --model <model-id>
acps agent set --model <model>
acps agent set --mode <mode>
acps agent set --effort <effort>
```

### Flags

- `--model <model>`, `--mode <mode>`, `--effort <effort>`: mapped values, validated against the configured agent's ACP-advertised options. Codex with OpenRouter validates `--effort` against the provider catalog's reasoning-effort values for the configured model and writes the pin into `~/.codex/config.toml`.
- `--custom-provider` with `--provider`, `--provider-name`, `--base-url`, `--api-key-ref`, `--model`: declare custom-provider metadata. Custom-provider model ids are accepted as supplied.

### Output

- For provider-backed agents, `--model` uses the existing `[agent.provider]` when present.
- When a change requires the supervised process to reload agent-owned config, the CLI prints a restart hint.

## `acps agent config`

Inspects or imports the configured harness's user-global config through the running daemon.

### Synopsis

```sh
acps agent config inspect <path> [--admin-key <key>]
acps agent config import <path> [--managed-field <id>]... [--ack-executable-settings] [--admin-key <key>]
```

### Flags

- `<path>`: the upload source only; the configured harness fixes the destination. Inputs are limited to 1 MiB.
- `--managed-field <id>` (repeatable): import only the compatible managed ids selected. Omitting managed ids preserves the current canonical provider, model, and MCP values.
- `--ack-executable-settings`: required when the inspection reports unmanaged settings that can execute commands or load code.
- `--admin-key <key>`: admin auth.

### Output

- `inspect` prints the redacted revision and field classifications.
- `import` repeats inspection, imports the selected managed ids, replaces the unmanaged residual, and regenerates managed native settings from canonical config.
- The command reports `applied` when the transaction completes, `queued` when restart blockers must clear first, and exits nonzero with the returned typed code on failure.

#### Supported sources and destinations

- Claude Code `settings.json` or `settings.local.json` → `~/.claude/settings.json`. A project-scope `settings.local.json` imports as user-scope settings.
- Codex CLI `config.toml` → `~/.codex/config.toml`.
- OpenCode `opencode.json` or `opencode.jsonc` → normalized JSON at `~/.config/opencode/opencode.json`.
- Amp Code `settings.json` → `~/.config/amp/settings.json`. Amp is provider-opaque and keeps its model in ACP session config rather than settings, so its import carries only MCP servers.
- Pi `settings.json` → `~/.pi/agent/settings.json`. Pi imports its `defaultProvider`/`defaultModel` selection and carries no MCP. Only `settings.json` is accepted: `models.json`/`auth.json` hold literal credentials with `!shell-command` exec, and `trust.json`/`mcp.json` are out of scope.
- Goose `config.yaml` → `~/.config/goose/config.yaml`. Goose imports its `GOOSE_PROVIDER`/`GOOSE_MODEL` selection plus `extensions` MCP servers. Only `config.yaml` is accepted: `secrets.yaml` and `permission.yaml` hold credentials and per-tool approvals.
- Kimi Code `config.toml` → `~/.kimi-code/config.toml`. Kimi imports the provider and model behind `default_model`, resolved by the referenced `[providers.<name>]` entry's `type` and `base_url` against the catalog rows Kimi runs. Only `config.toml` is accepted: `mcp.json` is out of scope and MCP reaches Kimi over ACP.
- Hermes Agent `config.yaml` → `~/.hermes/config.yaml`. Hermes imports `model.provider` and `model.default`; `mcp_servers` stays native in the residual. A `custom`, `ollama`, `vllm`, `llamacpp`, or `auto` provider is reported incompatible.

## `acps subagent`

Manages the OpenCode small-model lane. OpenCode-only.

### Synopsis

```sh
acps subagent status
acps subagent set ...
acps subagent match
acps subagent free
acps subagent disable
```

### Output

- `match` makes `small_model` follow the main agent model.

## `acps agent update`

Updates stale managed agent steps.

### Synopsis

```sh
acps agent update [--force] [--restart]
acps agent update set --auto-on
acps agent update set --auto-off
acps agent update set --frequency 3d
```

### Flags

- `--force`: update even when the daemon reports an active agent process. By default the update skips in that case.
- `--restart`: stop the running agent, update, then start it again. Requires the admin key.
- `--auto-on`, `--auto-off`: enable or disable automatic agent updates.
- `--frequency`: update schedule; accepts hour/day/week units (minimum 1 hour), e.g. `12h`, `1d`, `3d`, `4w`.

### Output

- `--restart` runs the update offline while the daemon is live. It must not overlap a scheduled daemon auto-update window: both write the same install destination and have no cross-process lock.
- A custom (non-registry) agent has nothing to update; the command reports a skip and exits 0. `update set` is rejected for a custom agent, which cannot be managed-updated.
- A configured `harness_version` pin constrains the update target the same way it constrains install: the pinned GitHub Release tag is resolved instead of the latest release (harness step, github path only), and a pinned agent already at its pin reports up-to-date.
- The same update runs in-daemon on demand via `POST /v1/agent/update` (see [api.md](../api/api.md)), which does hold the in-process update lock.

## `acps agent start`, `stop`, `restart`

Drives the supervised agent process through the running daemon.

### Synopsis

```sh
acps agent start [--admin-key <key>]
acps agent stop [--admin-key <key>]
acps agent restart [--admin-key <key>]
acps restart [--admin-key <key>]
acps restart auto [--admin-key <key>]
acps agent restart auto [--admin-key <key>]
```

### Flags

- `--admin-key <key>`: required; the daemon call is admin-tier.
- `auto`: queue a supervised-agent restart that runs once the target has no pending/running prompts and no pending ACP permission requests.

### Output

- `acps restart` is the preferred top-level alias for `acps agent restart`.
- Active sessions with no in-flight prompt are safe for `auto`; terminal latest prompts are safe.

## `acps agent status`, `acps agent check`

Reports configured agent state and managed install freshness.

### Synopsis

```sh
acps agent status
acps agent check
```

### Output

- `status` prints configured identity, capability summary, and recent lifecycle information. `acps agent provider list-active` reports the sanitized configured/loaded provider state.
- `check` reports whether managed install steps are present and current. A custom (non-registry) agent has no managed steps, so it reports a skip and exits 0.

## `acps agent test`

Sends a real prompt through the configured agent.

### Synopsis

```sh
acps agent test [--format json]
```

### Flags

- `--format json`: emit the machine-readable result document.

### Output

- The run may consume provider credits.
- The testflight is non-interactive. It auto-approves agent permission requests by selecting the first allow-kind option: allow-once is preferred over allow-always so no durable grant is left behind, and a reject option is never selected. A request offering no allow option is cancelled.
- The run is disposable. Before the agent process is shut down, the session it created is deleted through `session/delete` when the agent advertises that capability. `acps agent test` opens no state store, so it writes no session row.
- The `--format json` document contract: [cli.md](cli.md#acps-agent-test-json-contract).

## `acps agent default set`

Repoints the Array primary target.

### Synopsis

```sh
acps agent default set <target>
```

### Output

- Repoints the primary target at an existing target without touching the others, so the default `acps agent *` surfaces follow it.

## `acps array`

Manages multi-target Array mode: `status`, `on`, `off`, `add`, `set`, `provider`, `install`, `start`, `stop`, `restart`. Commands, flags, and effects: [array.md](../array.md#cli).

`acps array status` reads through the local read-only route; its JSON response also carries sanitized provider state.

## `acps sessions`

Lists, inspects, and drives inference sessions.

### Synopsis

```sh
acps sessions list [--range <day|week|month|year|all|duration>] [--range-start <datetime>] [--range-end <datetime>] [--limit <n>]
acps sessions status [--window <duration>] [--threshold <duration>] [--limit <n>]
acps sessions new [--session-key <key>]
acps sessions load <session-id> [--cwd <path>] [--session-key <key>]
acps sessions resume <session-id> [--cwd <path>] [--session-key <key>]
acps sessions fork <session-id> [--message-id <id>] [--cwd <path>] [--session-key <key>]
acps sessions prompt <session-id> [--session-key <key>]
acps sessions commands list <session-id> [--session-key <key>]
acps sessions commands run <session-id> <command> [args...] [--no-wait] [--timeout-secs <n>] [--session-key <key>]
acps sessions cancel <session-id> [--session-key <key>]
acps sessions close <session-id> [--session-key <key>]
```

### Flags

- `--range`, `--range-start`, `--range-end`, `--limit`: list filters.
- `--window` (default `8h`; accepts `1m` through `999h`): rolling activity window for `status`.
- `--threshold`: recency threshold for the `recent` field.
- `--cwd <path>`: session working directory. CWD values must be existing absolute directories that canonicalize under `[workspace].root`; stored CWD defaults are rechecked before load, resume, or fork.
- `--message-id <id>`: fork from an acknowledged prompt message id when the agent advertises that capability.
- `--no-wait`, `--timeout-secs <n>`: `commands run` polling controls.
- `--session-key <key>`: session-tier auth.

### Output

- `sessions list` shows the durable local session list after any supported ACP session-list sync. Sessions discovered from the agent but not loaded locally are shown as `available`.
- `sessions list` and `sessions status` use the local read-only socket and do not require a session key.
- `sessions status` prints sessions with activity in the window and a derived turn state such as `prompt_sent`, `working`, `permission_required`, `done`, or `error`.
- `sessions commands list` prints the slash commands the agent last advertised for the session over ACP.
- `sessions commands run` submits a slash command through `POST /v1/sessions/{id}/commands` and polls it like `sessions prompt`. A leading `/` on the command name is accepted. A command absent from the advertised list prints a warning but is still submitted.
- `sessions new`, `load`, `resume`, `fork`, `prompt`, `commands`, `cancel`, and `close` affect inference session state. They require `--session-key` or `ACP_STACK_SESSION_KEY` unless `[local].session_auth = "keyless"` is active.
- `sessions load` and `sessions resume` call the matching ACP session operation through the daemon. `sessions fork` creates a child session through ACP.
- `sessions close` closes the agent-side session and preserves local history; permanent deletion is deferred until product semantics are defined.

## `acps skills`

Manages the active agent's Agent Skills after init, through the running daemon. Init-time skills flags are under [`acps init`](#acps-init). Managed-marker and one-way-mirror semantics: [skills.md](../agents/skills.md).

### Synopsis

```sh
acps skills list [--session-key <key>]
acps skills catalog [--session-key <key>]
acps skills add <source> <selector>... [--admin-key <key>]
acps skills remove <name> [--admin-key <key>]
acps skills source get <source> [--session-key <key>]
acps skills source add <alias> <owner/repo> [--branch <b>] [--trusted] [--admin-key <key>]
acps skills source remove <alias> [--admin-key <key>]
```

### Flags

- `<source>`: a reviewed catalog alias (see [data/skills.toml](../../../data/skills.toml)), a configured user-source alias, `github:<owner>` for `<owner>/skills`, or `github:<owner>/<repo>` for a whole repo. Both github forms use branch `main`.
- `<selector>...`: one or more skill selectors.
- `<name>`: install name of one skill; a `/`-joined path for nested skills.
- `--branch <b>`: user-source branch, default `main`.
- `--trusted`: record that the user source has been vetted.
- `--session-key <key>` / `--admin-key <key>`: reads are session-tier; mutations are admin-tier.

### Output

- `list` shows the skills installed for the active agent, each with the source it was installed from. The source is read from the managed marker; hand-placed folders show `(unmanaged)`.
- `catalog` prints the embedded catalog sources and their selectors, plus configured user sources. User sources are shown with `(none indexed)`; enumerate those live with `source get`.
- Reads use `--session-key`/`ACP_STACK_SESSION_KEY`, falling back to the local read-only socket when `[local].session_auth = "keyless"` is active.
- `add` installs one or more skills; already-installed skills are skipped. `remove` uninstalls one skill. Removal needs no confirmation: only skills installed by acp-stack can be removed, and they are re-downloadable via `add`. Both mutations require the admin key and refresh the harness link directory afterward.
- `source get` fetches a source — a catalog alias, a configured alias, or `github:<owner>[/<repo>]` — and lists its installable skills with the `name` and `description` from each `SKILL.md`.
- `source add` registers a user source in `[[skills.sources]]`; the alias then works anywhere a source is accepted. It cannot shadow a catalog alias: the add is refused, and even a hand-edited collision is ignored in favor of the catalog.
- `source remove` unregisters the alias. `source add`/`remove` require the admin key and edit config through the daemon; they do not install or delete any skills.

## `acps status`

Validates the local instance and probes the daemon.

### Synopsis

```sh
acps status
```

### Output

- Validates local config and state, prints workspace and agent status, and probes daemon readiness through the local socket when the daemon is reachable.

## `acps logs`

Queries durable events and tails the live stream.

### Synopsis

```sh
acps logs query [filters] [--order <asc|desc>] [--category <category>] [--follow] [--json]
acps logs tail [--session-key <key>]
```

### Flags

- Filters: level, kind or kind prefix, source, session id, command id, permission id, security category, time bounds, and cursor.
- `--order <asc|desc>`: sort direction, default `desc`.
- `--json`: emit the `{ events, next_cursor }` envelope to stdout and suppress the human "more rows" hint.
- `--category <rate_limit|origin_cors|ip_block|oversized_request>`: scope to one security category.
- `--follow`: subscribes to the daemon's `logs` WebSocket topic, drains matching durable backlog in ascending pages, then continues with live events. Requires the session key.

### Output

- `logs query` reads durable events without a session key.
- With `--json --follow`, stdout is newline-delimited `EventJson` objects rather than the non-follow envelope.
- `logs tail` opens a WebSocket subscription to the running daemon and requires the session key.

## `acps metrics summary`

Prints daemon summary metrics.

### Synopsis

```sh
acps metrics summary
```

### Output

- Prints the daemon's summary metrics for a time window through the local read-only route.

## `acps ws`

Inspects and disconnects live WebSocket state.

### Synopsis

```sh
acps ws connections
acps ws sessions
acps ws disconnect [--admin-key <key>]
```

### Flags

- `--admin-key <key>`: required for `disconnect`.

### Output

- `connections` and `sessions` use the local read-only route.
- `disconnect` mutates live public WebSocket state and requires the admin key.

## `acps security`

Runs and inspects security self-checks.

### Synopsis

```sh
acps security check
acps security history [--limit N] [--after <id>] [--json]
acps security show <run-id> [--json]
```

### Flags

- `--limit N`, `--after <id>`: history paging.
- `--json`: machine-readable output.
- `history` and `show` require the admin key.

### Output

- `check` runs the security self-check through the local diagnostic route and persists the run to history.
- `history` lists prior runs newest-first. `show` prints a single recorded run with its findings.

## `acps deps`

Reports and applies declared dependencies.

### Synopsis

```sh
acps deps check
acps deps apply [--yes] [--admin-key <key>]
```

### Flags

- `--yes`: skip the confirmation prompt.
- `--admin-key <key>`: required for `apply`.

### Output

- `deps check` reports declared dependency status from local config.
- `deps apply` runs only install actions declared in config and requires confirmation unless `--yes` is passed.
- System-scope actions escalate through `sudo -n` when the process is non-root and passwordless sudo is available. Otherwise they are recorded as `privilege_required`. Text output then prints the manual `sudo <shell> -c '…'` commands, and the command exits non-zero — unlike init, which skips and continues. With `--format json`, the skips are reported through the error summary and exit code.
- Apply output includes the durable `apply_run_id`; failed runs point operators to `acps installer history --agent deps_apply`.

## `acps installer history`

Lists recorded installer runs.

### Synopsis

```sh
acps installer history [--agent <agent>]
```

### Flags

- `--agent <agent>`: filter by installer agent; `--agent deps_apply` selects dependency-apply runs.

### Output

- Dependency-apply output prints the durable `apply_run_id` for cross-reference with this history.

## `acps completion`

Writes a shell completion script.

### Synopsis

```sh
acps completion <bash|zsh|fish|powershell|elvish>
```

### Output

- Writes the completion script for the named shell to stdout.
