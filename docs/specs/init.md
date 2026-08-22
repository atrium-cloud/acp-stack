# Init

`acps init` initializes an `acp-stack` instance: it creates and validates config and state, initializes the encrypted secret store, generates API keys as non-recoverable state verifiers on first run, and optionally configures an agent, provider, workspace sources, MCP servers, Agent Skills, an edge profile, and a testflight. This document describes the end-to-end flow an operator goes through. The flag reference lives in [cli.md](cli.md#initialization); this guide describes the sequence and behavior those flags drive.

## Interactive And Non-Interactive

`acps init` runs interactively when stdin is a TTY and `--non-interactive` is not set. In that mode it prompts for missing choices. Agent, provider, and advertised model selectors are searchable. Environment configuration chooses a Standard or Advanced setup path, then asks per-item opt-in prompts; declining every prompt continues without environment changes. Esc and Ctrl-C abort init. When stdin is not a TTY, or `--non-interactive` is passed, every prompt is skipped and the corresponding value must be supplied by flag.

The non-interactive contract: a first run that creates a new config requires `--agent <id>`, the `--custom-agent-*` flag set, or a complete imported config. Provider, MCP, and agent env secret refs must resolve when used. A non-interactive first run with no agent path fails before writing config.

`acps init --handoff-json` is the platform automation handoff mode. It disables prompts, writes only one JSON object to stdout, and keeps the broader `acps init --format json` form rejected. Platform callers must provide the same required inputs as any other non-interactive init.

## The Resumable Run

Each `acps init` invocation is recorded as an init run. Within a run, every phase is recorded as a step keyed by an ordinal, so a failed or interrupted run can be continued from the first unsettled step. See [runtime.md](runtime.md) for the step machine and `src/runtime/init_runner.rs` for the implementation.

- `acps init --resume [--run-id <id>]` continues the most recent unfinished or failed run (or a specific run id). Completed steps whose postcondition still holds are replayed as skipped; the first failed or incomplete step re-runs, and everything after it runs fresh. The original run's arguments replay, with anything passed on the resume invocation taking precedence, and a recorded `--provider`, `--model`, `--mode`, or `--effort` re-runs provider configuration rather than replaying it as skipped, so the value is validated and persisted.
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
    - While a managed-state endpoint override is stored, an agent apply is rejected when the target cannot carry the override: a registry agent without `set_provider_base_url`, any custom agent, or a re-confirmed agent whose kept provider is the overridden one on a pair that refuses overrides (codex + `openai`). Clear the namespace's credential endpoint first; see [extensions.md](extensions.md#type-managed-state).
4. Environment configuration (new config only).
    a. Standard setup
        - Install essential dependencies including `nodejs`, `python` 3.14, `git` (yes/no)
        - Install `browser-use` (yes/no)
        - Add essential agent skills (yes/no) -(if yes)-> add Anthropic `docx`, `pptx`, `xlsx`, and `pdf`, plus OpenAI plugin skills `gh-address-comments`, `gh-fix-ci`, `github`, and `yeet`
        - Add data sources (now/later) -(if now)-> add a local path, HTTPS archive/download, or S3 bucket
    b. Advanced setup
        - Install custom dependencies (add dependency/skip)
        - Add agent skills (now/later) -(if now)-> search the checked-in reviewed skill sources
        - Add agent env (now/later)
        - Add data sources (now/later) -(if now)-> add a local path, HTTPS archive/download, or S3 bucket
    MCP prompting runs after agent install as its own step (step 12); flag-declared MCP servers (`--mcp-stdio`, `--mcp-http`, `--mcp-preset`) still land in the starter config here.
5. Config and state.
    - Write a starter config or validate the existing/imported config.
    - Open SQLite state and run migrations.
    - For supported registry agents:
        - New selections default to `[agent.auto_update] enabled = true`, `frequency = "1d"`; `--agent-update <on|off>` / `--agent-update-frequency <freq>` (or the interactive prompt) override it, so auto-update can be declined at init instead of only afterward via `acps agent update set --auto-off`. Custom (non-registry) agents cannot auto-update, so `--agent-update on` is rejected.
        - Re-confirming the same agent preserves policy.
        - Switching agents resets to the supported-agent default.
6. Agent Skills selection (interactive, when selected).
    - Choose a reviewed catalog source or `github:<owner>`.
    - Choose individual skill selectors to install before testflight.
    - `--skills-source`, `--skills`, and `--no-skills` drive this without prompts.
7. Secrets and auth.
    - Generate session and admin API keys when no auth verifier rows exist.
    - Preserve existing verifier rows on re-run.
    - Show fresh plaintext keys once at final handover.
    - Store interactive agent env values and verify `--agent-env-ref` names before install.
    - For a Kilo config that authenticates through a provider-native credential (declared via `--agent-env-ref` or present in an imported config), seed the `KILO_API_KEY` declaration when missing and record an empty placeholder for it automatically: the harness requires the variable present even with a non-Kilo provider, so no separate `secrets set` is needed. `acps config import` and `acps agent set --model` apply the same rule outside init.
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
    - System-scope actions run directly as root, through `sudo -n` when the process is non-root and passwordless sudo is available, and are otherwise skipped with a warning listing the manual `sudo <shell> -c '…'` command per action and the `acps init --resume --deps-apply --deps-apply-yes` follow-up.
    - Privilege skips are recorded as `privilege_required` under `deps_apply`, surface in status/health, and do not fail init; genuine action failures still fail init.
11. Capability probe.
    - Spawn the installed agent for a handshake-only ACP `initialize`, record the advertised capabilities to state, and terminate the process; no session is created. `GET /v1/agent/capabilities` and `acps agent status` answer from this snapshot before the agent's first start.
    - Configured features the advertisement does not cover are reported as ignored; they stay in config and are skipped at session time.
    - A failed probe records `probe_status: "unavailable"` and never fails init. The step re-runs on every resume.
    - The probe runs before provider configuration and harness config provisioning; session-time behavior always follows the live bridge.
12. MCP configuration (interactive, new config only, skipped when MCP servers were already declared by flag or in the hosted start request).
    - Prompts run only when the probe advertised MCP support; the transport picker offers HTTP only when `mcpCapabilities.http` is advertised. Added servers are written to config and their secret refs collected.
    - Hosted sessions get the same prompts on the stream, each carrying its machine-readable kind. Secret values are requested only as `password` inputs; refs and header templates are ordinary text inputs, screened at the boundary so a pasted credential is rejected without being echoed.
    - Skips are reported as progress, so a hosted client learns why the picker never appeared (probe unavailable, or the agent does not advertise MCP support).
    - Resume never re-drives these prompts.
13. Provider, model, mode, and effort.
    - Supported registry agents:
        - Select or validate provider and required secret refs. A ref is satisfied by the flat secret store or by a structured catalog credential covering it (registry providers via their canonical mapping, custom providers via the configured `api_key_ref`); the provider picker's readiness labels and the resume-time idempotence check use the same rule.
        - Discover ACP-advertised model, mode, and reasoning-effort options with one provisional session shared by all lanes. Effort values come from the agent's `thought_level` session config option.
        - The mode and effort lanes run only for agents the registry marks as supporting them; `--mode` or `--effort` against any other registry agent is rejected before discovery, as `--model` is. Provider-backed agents need a provider (passed this run or already in config) before any lane runs, since the harness cannot be launched to advertise anything without one.
        - Interactive runs offer mode and effort selectors alongside the model selector. A non-interactive run without `--mode`/`--effort` never enters that lane: it spawns nothing, prints nothing, and writes no value. Explicit `--mode` and `--effort` are validated against the advertised values and a rejection lists them.
        - When mode and/or effort are the only active lanes and neither flag was passed, a provisional session that cannot be established is reported and skipped rather than failing init; an explicit `--mode`/`--effort` still fails loudly.
        - Discovery also requires a resolvable provider credential: when a configured custom provider's api-key ref is still pending a managed credential push, all lanes skip with a progress note naming the provider and ref, and an explicit `--mode`/`--effort` fails early with the same remediation rather than at the spawn. An explicit `--model` for the primary agent's custom provider bypasses discovery and is written as supplied, so it only trips the early failure when the pending custom provider is the subagent's.
        - Codex with a non-OpenAI provider lists models from the provider's live catalog (fetched during init and cached at `~/.config/acp-stack/provider-models.json`) instead of the adapter's advertised OpenAI presets; when no catalog is available (custom provider or an offline fetch), the model step is skipped with a hint to rerun with `--model`.
        - Apply `--provider`, `--api-key-ref`, `--model`, `--mode`, `--effort`, and custom-provider flags. A custom provider's id must not be one the mapped-provider registry already knows, including ids the registry maps for other harnesses: registry ids are reserved instance-wide, so codex with an Anthropic-compatible endpoint uses a distinct id such as `anthropic-1`. Passing a registry id with `--custom-provider`, or selecting a registry id that has no key mapping for the chosen agent, is rejected with that remediation.
        - Kimi Code skips model discovery: `--model` is accepted as supplied, and without it init pins the selected provider lane's default (`kimi-for-coding` on the subscription lanes, `kimi-k3` on the Moonshot platform) unless config already has a model. Provider selection follows the standard provider-backed rules; at runtime, a legacy config without `[agent.provider]` launches on the Kimi For Coding subscription lane.
    - Custom agents:
        - Skip provider/model/mode/effort discovery.
        - Run one ACP connection gate when the launch command and cwd are present.
        - Explicit `--model`, `--mode`, and `--effort` are rejected.
14. `acp-stack` auto-update.
    - Configure `[updates.acp_stack]` as on, security-only, or off.
    - Frequencies use day/week units, minimum `1d`.
    - Explicit `--stack-update` flags apply on any run.
    - Existing configs skip the prompt when no stack-update flags are supplied.
15. Agent-owned config.
    - Write supported-agent config files for headless API-key use.
16. Edge artifacts.
    - For `--edge cloudflare`, write generated tunnel artifacts or provision managed tunnel refs.
17. Init complete.
    - Record the durable completion event.
18. Testflight (optional).
    - See Testflight.

After the steps settle, init prints a summary: the config, state, secret-store, and age-key paths, and the auth status. Terminal runs also print one `ignored:` line per feature the probe found unsupported; hosted runs surface these only through the handoff payload's `ignored_features`.

## Key Handover

When no auth verifier rows exist, init generates two API keys and shows their plaintext values to the operator exactly once:

- Session key — session-driving and prompt-driving API calls.
- Admin key — secrets, config import, agent process control, and other elevated operations.

The handover prints the two values. The values are never stored in plaintext, never returned through the API, and never reprinted on a later run: a re-run or `--resume` over existing verifier rows takes the preserved path and shows nothing. Save them when shown. Successful text-mode runs end with a next-step hint pointing at `acps serve` and `acps sessions new`, since init itself leaves no daemon running; the hint prints on the preserved path too, but not when a failed run renders keys through the drop guard.

`acps init --rotate-keys` regenerates both keys in place over existing verifier rows and shows the new plaintexts once, exactly like a fresh generation; the retired keys stop verifying immediately. A running daemon caches the verifier pair at startup, so it must be restarted before the rotated keys are accepted. Without `--rotate-keys`, `acps reset --yes` remains the only rotation path.

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

`session_key` and `admin_key` appear only when that invocation freshly generated or rotated the keys (rotated keys are reported under `generated_keys`). A later run without `--rotate-keys` preserves the verifier rows and reports `"preserved_keys": ["session", "admin"]` without reprinting either plaintext key. If init fails after fresh key generation, handoff mode emits the same shape with `"status": "failed"` so automation can capture the one-time keys before retrying.

The payload carries an `ignored_features` array (omitted when empty) listing configured features the capability probe found unsupported: `[{"feature": "mcp.server", "value": "linear", "capability": "mcpCapabilities.http", "reason": "..."}]`.

## Hosted Streaming Init

`acps init serve` runs a bootstrap-only HTTP/WebSocket server for hosted init. The hosted backend connects to the instance; the web UI does not connect to the instance directly. Bootstrap auth uses a bearer token from `ACP_STACK_INIT_TOKEN`, `--token-env`, or `--token-file`.

The hosted flow follows the same init steps as interactive `acps init`, but only streams the bootstrap prompts needed for agent selection, provider selection, required secret collection, custom-provider fields, model, mode, and effort selection, the post-install MCP setup, and the simple confirmations on that path. Every streamed prompt carries a machine-readable `kind` and, for selections, stable option values, so a client routes and answers by id rather than by prompt text. Environment configuration (skills, dependencies, browser-use, data sources) and the acp-stack/agent auto-update policies (`stack_update`/`stack_update_frequency`, `agent_update`/`agent_update_frequency`) are declared up-front in the session-create request instead of being streamed: these wizard prompts remain outside the streamed set, and the request fields map onto the same init arguments the wizard would produce. Secret collection covers the refs those declarations name — MCP env/header refs (whole-value refs and refs named inside `${}` templates alike) and S3 data-source key refs missing from the store are requested as `password` inputs; an unanswered ref skips without failing init and surfaces later through MCP health or workspace materialization. Custom-provider api-key refs defer only when the start request declared `defer_provider_credentials: true`: on such a run a null answer to the `provider_api_key_value` prompt leaves the ref pending, init reports the deferral as progress and continues, and the credential is expected through the managed-state extension after init completes — until it arrives, agent start fails with an error naming the provider and ref. The declaration is the caller's promise to push the credential out-of-band; without it a driven init keeps the hard failure just like mapped providers, terminal runs, and non-interactive runs, so the deferral is a stated contract rather than an inference from the transport. Normal `acps init` keeps its existing terminal behavior; hosted prompts outside the streamed set use the same skip/default behavior as non-interactive init unless supplied through initial args. The post-install MCP configuration step streams its prompts only when the start request declared no MCP server — declaring servers up front still wins and skips the wizard outright; MCP declarations the installed agent's capabilities do not cover are reported only through the result frame's `ignored_features`, never through progress frames.

Alongside the prompt stream, the session reports structured readiness through `signal` events: each is one raw fact — a step starting or finishing, or one of the ten categories (agent, provider, model, mode, effort, workspace, native config, MCP, skills, dependencies) becoming applicable, settled, or failed. The instance forwards the facts and does not derive a rendered view; the client folds the stream into one. The whole signal stream so far rides the `hello` frame and the status response, so a client that connects late folds the same input without parsing progress text. The signal shapes, the fold's category vocabulary, and its status values are in [api.md](api/api.md#bootstrap-init-api).

A hosted session can also continue a crashed run and bring its own agent: `resume`/`fresh` in the start request behave like `--resume`/`--fresh`, and the `custom_agent_*` fields declare an escape-hatch agent the way the `--custom-agent-*` flags do. Custom-agent prompts are not streamed, so that declaration must be complete in the request; the instance emits `category_applicability` signals marking provider, model, mode, effort, and skills not applicable, and the client fold renders them so, since a non-registry agent configures those through its own environment.

A fresh registry-agent session may include one in-memory `native_config` upload. The server inspects it before durable init work and sends a `native_config_review` input containing only the redacted manifest. The response selects compatible managed field ids and acknowledges executable unmanaged categories for that revision. After agent installation, init commits the journaled semantic replacement before provider/model discovery and before the first persistent agent start, so onboarding does not restart an agent. The source document is excluded from recorded init arguments, events, progress, and handoff metadata; the final handoff may include only the sanitized `native_config_import` operation.

While a result is awaiting acknowledgement, `POST /v1/init/sessions/{id}/native-config/cancel` with `{ "operation_id", "revision" }` rolls back an applied onboarding import: the server restores the journaled pre-import snapshots and returns the operation with status `cancelled`. The backend uses this as a compensating action when it cannot validate or persist the reported operation; retries are idempotent. Unlike the runtime cancel route, this rollback does not gate on applied-file digests, because later init steps legitimately rewrite the canonical config after the onboarding apply.

Final result delivery uses the same handoff payload shape as `--handoff-json`. Hosted init always rotates existing keys: plaintext session/admin keys travel only in the WebSocket `result` frame, so a preserving run would leave the backend permanently unable to obtain credentials for an instance whose state predates the session. The result frame therefore always carries `session_key` and `admin_key`. Status and event replay report only non-secret state. The backend must send `ack_result` after storing or forwarding the keys. After acknowledgement, the in-memory result is cleared, the session closes, and the bootstrap server exits successfully.

A failure after key handover still delivers a `"status": "failed"` result frame through the same result/ack path. A failure before key handover parks instead of exiting immediately: the session turns `errored`, the typed error stays retrievable (status route, reconnect `hello`, `replay_error` frame), and the backend sends `ack_error` to release the server, which then exits non-zero. An unacknowledged error expires after a 2-minute grace (reason `error_ack_timeout`) even while a WebSocket is connected, and the server still exits non-zero.

The server bounds its own lifetime so an abandoned bootstrap cannot pin the bind port forever. When no WebSocket client is connected and no API call has arrived for `--idle-timeout` (default `15m`; `0s` disables), the session is cancelled with reason `idle_timeout`; `--max-lifetime` (disabled by default) does the same with reason `max_lifetime` once the absolute cap elapses, regardless of activity. A WebSocket disconnect restarts the idle clock, so a dropped backend gets the full timeout to reconnect and acknowledge. Expiry also fires from `completed_awaiting_ack`: the stored un-acknowledged result is discarded and zeroized, and the process exits non-zero. A parked `errored` session has its own 2-minute `error_ack_timeout` grace that runs regardless of the idle setting and of connected clients. Reaching either limit before any session was created also exits non-zero; the pre-session idle clock runs from the last authenticated API call, not just server start. When a session turns terminal, the server closes any attached WebSocket after forwarding the final event, so a hung client cannot hold the process past `--max-lifetime`. A session status snapshot includes `last_activity_age_secs` — the idle time leading up to that status request, before the request itself counts as activity — so the hosting backend can make its own reap-vs-wait decisions.

## Testflight

After config and secrets are present, init can run a testflight that starts the configured agent and sends a minimal real prompt to verify the connection end to end — session creation, prompt completion, streamed updates, and a terminal prompt state, plus at least one filesystem-visible tool action when the agent supports tools. Testflight is opt-in because it may consume provider credits:

- Interactive runs prompt with a credit warning before running.
- `--testflight` runs it without prompting; `--skip-testflight` skips it.
- Non-interactive runs skip testflight unless `--testflight` is passed.
- A configured custom provider whose api-key ref is still pending a managed credential push skips testflight, naming the provider and ref; an explicit `--testflight` fails with the same remediation instead of spending a prompt on an unresolvable credential.
- A hosted run whose backend answers the credit-warning prompt with `value: false` and `deferred: true` (see [api.md](api/api.md)) reports `testflight: deferred (runs after setup)` rather than a decline: the backend intends to run the test itself once setup completes.

Testflight hard-fails on unsupported paths (browser-OAuth agents, private Drive/Dropbox links, non-archive cloud folders, unsafe archives, missing required secrets) and fails if an agent appears active but emits no progress or terminal state within the configured timeout.

## Related

- [cli.md](cli.md#initialization) — the `acps init` flag reference.
- [config.md](config.md) — the config schema init writes.
- [runtime.md](runtime.md) — the resumable step machine and workspace materialization.
- [security.md](security.md) — key generation and the admin-key policy.
- [agents/](agents/) — per-agent install, launch, and auth setup.
