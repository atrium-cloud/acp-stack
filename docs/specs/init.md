# Init

`acps init` initializes an `acp-stack` instance: it creates and validates config and state, initializes the encrypted secret store, generates the API keys on first run, and optionally configures an agent, provider, workspace sources, MCP servers, Agent Skills, an edge profile, and a testflight. This document describes the end-to-end flow an operator goes through. The flag reference lives in [cli.md](cli.md#initialization); this guide describes the sequence and behavior those flags drive.

## Interactive And Non-Interactive

`acps init` runs interactively when stdin is a TTY and `--non-interactive` is not set. In that mode it prompts for missing choices. Agent, provider, and advertised model selectors are searchable. Optional prompts expose an explicit Skip choice; Esc and Ctrl-C abort init. When stdin is not a TTY, or `--non-interactive` is passed, every prompt is skipped and the corresponding value must be supplied by flag.

The non-interactive contract: a first run that creates a new config requires either `--agent <id>` or a complete imported config, plus resolvable secret references for any provider or MCP secret refs. A non-interactive first run with neither fails before writing any config rather than leaving a placeholder agent on disk.

## The Resumable Run

Each `acps init` invocation is recorded as an init run. Within a run, every phase is recorded as a step keyed by an ordinal, so a failed or interrupted run can be continued from the first unsettled step. See [runtime.md](runtime.md) for the step machine and `src/runtime/init_runner.rs` for the implementation.

- `acps init --resume [--run-id <id>]` continues the most recent unfinished or failed run (or a specific run id). Completed steps whose postcondition still holds are replayed as skipped; the first failed or incomplete step re-runs, and everything after it runs fresh.
- `acps init --fresh` forces a new run rather than resuming.
- Re-running `acps init` over an already-initialized instance preserves existing API keys and config; it does not regenerate keys or overwrite config unless an explicit option requests it.

A failed step records the typed error and preserves any captured stdout/stderr in per-step log files for audit. Init fails fast: a step error halts the run and is reported, not silently skipped.

## Flow

The operator-facing sequence, in order:

1. Config source. The operator imports an existing config, resumes an interrupted init, or starts fresh. Imports accept an `acps-config.toml` path, scripted TOML text, or base64-encoded `acps-config.toml` text for safer terminal paste; the interactive source prompt offers file import and base64 paste. Imported values are authoritative; later steps run only when required fields or optional sections are missing and the operator chooses to fill them.
2. Path and registry preflight. Init resolves the config, state, age-key, and secret-store paths under `~/.config/acp-stack` and `~/.local/share/acp-stack`, creates the owner-only directories, and loads the embedded agent registry (with the optional operator override at `~/.config/acp-stack/agents.toml`).
3. Agent selection (new config only). If no `--agent` was passed, interactive runs present searchable supported registry agents and the operator picks one; non-interactive runs require the flag. Unsupported agents (browser-OAuth, non-headless) are rejected here, before any install.
4. Starter-config selections (new config only, interactive). Init offers, as add-loops, starter code sources (Git repos), data sources (local paths or HTTPS archives), the Linear MCP preset, custom stdio/HTTP MCP servers, and the secret references those MCP servers require. Flags (`--code-from`, `--data-from`, `--mcp-*`) pre-populate these and skip the matching prompt.
5. Config and state. Init writes the starter config (or validates an existing/imported one), opens the SQLite state store, and runs migrations.
   Supported registry agents get `[agent.auto_update] enabled = true` with `frequency = "1d"`. Re-confirming the same agent preserves an existing policy; switching agents resets to the supported-agent default.
6. Agent Skills selection (interactive). Init offers an Agent Skills source (OpenAI, Anthropic, or a custom GitHub owner) and a skill list, installed before testflight. `--skills-source`/`--skills`/`--no-skills` drive this non-interactively.
7. Secrets. Init generates the session and admin API keys on a fresh store, or preserves them on a re-run. The plaintext keys are held for the final handover and shown exactly once (see Key Handover).
8. Agent install. Init installs the configured agent from the embedded catalog; adapter-backed agents install both the harness and the adapter. Expected-hash verification runs when configured.
9. Workspace materialization. Code sources are cloned into `/workspace/usr/code/<repo>/` and data sources placed under `/workspace/usr/data/<name>/`, with archive-extraction safety checks.
10. Provider, model, and mode. Agents that support provider setup require a selected or pre-existing provider; agents that do not support provider setup skip this step. Interactive runs present searchable compatible providers — grouped by whether their secret refs are already available — and the operator picks one by entry or exact id. Required provider secret values are collected before model discovery. Existing/imported provider blocks are also checked for missing secret refs before discovery. Model and mode are then selected from, or validated against, the agent's ACP-advertised session config options. Flags `--provider`, `--api-key-ref`, `--model`, `--mode`, and the `--custom-provider` family drive this non-interactively.
11. Agent-owned config. Init writes the supported agent's own config files so env-injected API keys are consumed headlessly.
12. Edge artifacts. When `--edge cloudflare` is set, init writes the Cloudflare Tunnel artifacts (generated mode) or provisions the tunnel from secret refs (managed mode).
13. Init complete. A durable completion event is recorded.
14. Testflight (optional). See Testflight.

After the steps settle, init prints a summary: the config, state, secret-store, and age-key paths, and the auth status.

## Key Handover

On a fresh store, init generates two API keys and shows their plaintext values to the operator exactly once:

- Session key — normal sessions, workspace, commands, logs, and status.
- Admin key — secrets, config import, agent process control, and other elevated operations.

The handover prints the two values with the reminder that the admin key is never regenerable and that `acps reset --yes` is the only way to rotate it by reinitializing the instance. The values are never stored in plaintext, never returned through the API, and never reprinted on a later run: a re-run or `--resume` over an existing store takes the preserved path and shows nothing. Save them when shown.

## Testflight

After config and secrets are present, init can run a testflight that starts the configured agent and sends a minimal real prompt to verify the connection end to end — session creation, prompt completion, streamed updates, and a terminal prompt state, plus at least one filesystem-visible tool action when the agent supports tools. Testflight is opt-in because it may consume provider credits:

- Interactive runs prompt with a credit warning before running.
- `--testflight` runs it without prompting; `--skip-testflight` skips it.
- Non-interactive runs skip testflight unless `--testflight` is passed.

Testflight hard-fails on unsupported paths (browser-OAuth agents, private Drive/Dropbox links, non-archive cloud folders, unsafe archives, missing required secrets) and fails if an agent appears active but emits no progress or terminal state within the configured timeout.

## Related

- [cli.md](cli.md#initialization) — the `acps init` flag reference.
- [config.md](config.md) — the config schema init writes.
- [runtime.md](runtime.md) — the resumable step machine and workspace materialization.
- [security.md](security.md) — key generation and the admin-key policy.
- [agents/](agents/) — per-agent install, launch, and auth setup.
