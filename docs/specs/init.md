# Init

`acps init` initializes an `acp-stack` instance: it creates and validates config and state, initializes the encrypted secret store, generates API keys as non-recoverable state verifiers on first run, and optionally configures an agent, provider, workspace sources, MCP servers, Agent Skills, an edge profile, and a testflight. This document describes the end-to-end flow an operator goes through. The flag reference lives in [cli.md](cli.md#initialization); this guide describes the sequence and behavior those flags drive.

## Interactive And Non-Interactive

`acps init` runs interactively when stdin is a TTY and `--non-interactive` is not set. In that mode it prompts for missing choices. Agent, provider, and advertised model selectors are searchable. Environment configuration chooses a Standard or Advanced setup path, then asks per-item opt-in prompts; declining every prompt continues without environment changes. Esc and Ctrl-C abort init. When stdin is not a TTY, or `--non-interactive` is passed, every prompt is skipped and the corresponding value must be supplied by flag.

The non-interactive contract: a first run that creates a new config requires `--agent <id>`, the `--custom-agent-*` flag set, or a complete imported config. Provider, MCP, and agent env secret refs must resolve when used. A non-interactive first run with no agent path fails before writing config.

`acps init --handoff-json` is the platform automation handoff mode. It disables prompts, writes only one JSON object to stdout, and keeps the broader `acps init --format json` form rejected. Platform callers must provide the same required inputs as any other non-interactive init.

## The Resumable Run

Each `acps init` invocation is recorded as an init run. Within a run, every phase is recorded as a step keyed by an ordinal, so a failed or interrupted run can be continued from the first unsettled step. See [runtime.md](runtime.md) for the step machine and `src/runtime/init_runner.rs` for the implementation.

- `acps init --resume [--run-id <id>]` continues the most recent unfinished or failed run (or a specific run id). Completed steps whose postcondition still holds are replayed as skipped; the first failed or incomplete step re-runs, and everything after it runs fresh.
- `acps init --fresh` forces a new run rather than resuming.
- Re-running `acps init` over an already-initialized instance preserves existing API keys and config; it does not regenerate keys or overwrite config unless an explicit option requests it.

A failed step records the typed error and preserves any captured stdout/stderr in per-step log files for audit. Init fails fast: a step error halts the run and is reported, not silently skipped.

## Flow

The operator-facing sequence, in order:

1. Config source.
    - Resume, import, or start fresh.
    - Imports accept file path, TOML text, or base64 TOML. Imported values are authoritative.
    - Later prompts run only for missing required fields or opted-in optional sections.
2. Path and registry preflight.
    - Resolve config, state, age-key, and secret-store paths.
    - Create owner-only directories.
    - Load the embedded registry and optional `~/.config/acp-stack/agents.toml` override.
3. Agent selection (new config only).
    - Registry agent:
        - Interactive runs show searchable supported agents.
        - Non-interactive runs use `--agent <id>`.
        - Unsupported registry agents are rejected before install.
    - Custom agent:
        - Interactive runs offer "Custom agent...".
        - Non-interactive runs use `--custom-agent-*`.
        - The id must not be a registry agent id.
        - Provider/model setup is handled by the agent environment, not init flags.
4. Environment configuration (new config only).
    a. Standard setup
        - Install essential dependencies including `nodejs`, `python` 3.14, `git` (yes/no)
        - Install `browser-use` (yes/no)
        - Add essential agent skills (yes/no) -(if yes)-> add Anthropic `docx`, `pptx`, `xlsx`, and `pdf`, plus the OpenAI `github` plugin bundle
        - Add data sources (now/later) -(if now)-> add a local path, HTTPS archive/download, or S3 bucket
    b. Advanced setup
        - Install custom dependencies (add dependency/skip)
        - Add agent skills (now/later) -(if now)-> search checked-in skill and plugin-bundle snapshots
        - Add MCP servers (add MCP/skip)
        - Add agent env (now/later)
        - Add data sources (now/later) -(if now)-> add a local path, HTTPS archive/download, or S3 bucket
5. Config and state.
    - Write a starter config or validate the existing/imported config.
    - Open SQLite state and run migrations.
    - For supported registry agents:
        - New selections get `[agent.auto_update] enabled = true`, `frequency = "1d"`.
        - Re-confirming the same agent preserves policy.
        - Switching agents resets to the supported-agent default.
6. Agent Skills selection (interactive, when selected).
    - Choose OpenAI skills, Anthropic skills, OpenAI plugin bundles, or `github:<owner>`.
    - Choose skills or plugin bundles to install before testflight.
    - `--skills-source`, `--skills`, `--plugins-source`, `--plugins`, and `--no-skills` drive this without prompts.
7. Secrets and auth.
    - Generate session and admin API keys when no auth verifier rows exist.
    - Preserve existing verifier rows on re-run.
    - Show fresh plaintext keys once at final handover.
    - Store interactive agent env values and verify `--agent-env-ref` names before install.
8. Agent install.
    - Registry agents install from the embedded catalog.
    - Custom agents install through `[agent.install]`.
    - Adapter-backed agents install both harness and adapter unless the catalog marks the harness as adapter-provided.
    - Init prepares `workspace.root` and `workspace.uploads` before installer subprocesses run so installers have a valid working directory.
    - Expected-hash checks run when configured.
    - Retry uses bounded exponential backoff, with each attempt recorded in installer history.
9. Workspace materialization.
    - Clone code sources into `/workspace/usr/code/<repo>/`.
    - Place data sources under `/workspace/usr/data/<name>/`.
    - Apply archive-extraction safety checks.
10. Dependency install (optional).
    - Pending actions are `[dependencies.commands]` entries whose `creates` target does not resolve.
    - Interactive runs ask for confirmation and show system-scope notes.
    - Non-interactive runs require `--deps-apply --deps-apply-yes`.
    - Failures and unmet system privilege fail init and are recorded under `deps_apply`.
11. Provider and model.
    - Supported registry agents:
        - Select or validate provider and required secret refs.
        - Discover ACP-advertised model options with one provisional session.
        - Apply `--provider`, `--api-key-ref`, `--model`, and custom-provider flags.
    - Custom agents:
        - Skip provider/model discovery.
        - Run one ACP connection gate when the launch command and cwd are present.
        - Explicit `--model` is rejected.
12. `acp-stack` auto-update.
    - Configure `[updates.acp_stack]` as on, security-only, or off.
    - Frequencies use day/week units, minimum `1d`.
    - Explicit `--stack-update` flags apply on any run.
    - Existing configs skip the prompt when no stack-update flags are supplied.
13. Agent-owned config.
    - Write supported-agent config files for headless API-key use.
14. Edge artifacts.
    - For `--edge cloudflare`, write generated tunnel artifacts or provision managed tunnel refs.
15. Init complete.
    - Record the durable completion event.
16. Testflight (optional).
    - See Testflight.

After the steps settle, init prints a summary: the config, state, secret-store, and age-key paths, and the auth status.

## Key Handover

When no auth verifier rows exist, init generates two API keys and shows their plaintext values to the operator exactly once:

- Session key — session-driving and prompt-driving API calls.
- Admin key — secrets, config import, agent process control, and other elevated operations.

The handover prints the two values with the reminder that the admin key is never regenerable and that `acps reset --yes` is the only way to rotate it by reinitializing the instance. The values are never stored in plaintext, never returned through the API, and never reprinted on a later run: a re-run or `--resume` over existing verifier rows takes the preserved path and shows nothing. Save them when shown.

## Platform Handoff JSON

`acps init --handoff-json` emits the paths and keys a hosted platform needs after init:

```json
{
  "status": "initialized",
  "config_path": "/home/acps/.config/acp-stack/acps-config.toml",
  "state_path": "/home/acps/.local/share/acp-stack/state.sqlite",
  "secret_store_path": "/home/acps/.local/share/acp-stack/secrets.age",
  "age_key_path": "/home/acps/.config/acp-stack/age.key",
  "agent": {
    "id": "opencode",
    "name": "OpenCode"
  },
  "auth": {
    "generated_keys": ["session", "admin"],
    "preserved_keys": []
  },
  "session_key": "acps_...",
  "admin_key": "acps_..."
}
```

`session_key` and `admin_key` appear only when that invocation freshly generated the keys. A later run preserves the verifier rows and reports `"preserved_keys": ["session", "admin"]` without reprinting either plaintext key. If init fails after fresh key generation, handoff mode emits the same shape with `"status": "failed"` so automation can capture the one-time keys before retrying.

## Hosted Streaming Init

`acps init serve` runs a bootstrap-only HTTP/WebSocket server for hosted init. The hosted backend connects to the instance; the web UI does not connect to the instance directly. Bootstrap auth uses a bearer token from `ACP_STACK_INIT_TOKEN`, `--token-env`, or `--token-file`.

The hosted flow follows the same init steps as interactive `acps init`, but v1 only streams the bootstrap prompts needed for agent selection, provider selection, required secret collection, custom-provider fields, model selection, and the simple confirmations on that path. Normal `acps init` keeps its existing terminal behavior; hosted prompts outside the v1 scope use the same skip/default behavior as non-interactive init unless supplied through initial args.

Final result delivery uses the same handoff payload shape as `--handoff-json`. Status and event replay report only non-secret state; plaintext session/admin keys are sent only in the WebSocket `result` frame. The backend must send `ack_result` after storing or forwarding the keys. After acknowledgement, the in-memory result is cleared, the session closes, and the bootstrap server exits successfully.

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
