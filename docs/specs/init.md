# Init

`acps init` initializes an `acp-stack` instance. It:

- Creates and validates config and state.
- Initializes the encrypted secret store.
- Generates API keys as non-recoverable state verifiers on first run.
- Optionally configures an agent, provider, workspace sources, MCP servers, Agent Skills, an edge profile, and a testflight.

The flag reference lives in [cli.md](cli/cli.md#initialization). This guide describes the sequence and behavior those flags drive.

## Interactive And Non-Interactive

`acps init` runs interactively when stdin is a TTY and `--non-interactive` is absent. In that mode:

- It prompts for missing choices.
- Agent, provider, and advertised model selectors are searchable.
- Environment configuration chooses a Standard or Advanced setup path, then asks per-item opt-in prompts.
- Declining every prompt continues, keeping the environment as found.
- Esc and Ctrl-C abort init.

When stdin is redirected, or `--non-interactive` is passed, every prompt is skipped and the corresponding value must be supplied by flag.

The non-interactive contract:

- A first run that creates a new config requires `--agent <id>`, the `--custom-agent-*` flag set, or a complete imported config.
- Provider, MCP, and agent env secret refs must resolve when used.
- A non-interactive first run with no agent path fails before writing config.

`acps init --handoff-json` is the platform automation handoff mode. It disables prompts, writes only one JSON object to stdout, and keeps the broader `acps init --format json` form rejected. Platform callers must provide the same required inputs as any other non-interactive init.

## The Resumable Run

Each `acps init` invocation is recorded as an init run. Within a run, every phase is recorded as a step keyed by an ordinal, so a failed or interrupted run can be continued from the first unsettled step. See [runtime.md](runtime.md) for the step machine and `src/runtime/init_runner.rs` for the implementation.

- `acps init --resume [--run-id <id>]` continues the most recent unfinished or failed run (or a specific run id).
    - Completed steps whose postcondition still holds are replayed as skipped.
    - The first failed or incomplete step re-runs, and everything after it runs fresh.
    - The original run's arguments replay, with anything passed on the resume invocation taking precedence.
    - A recorded `--provider`, `--model`, `--mode`, or `--effort` re-runs provider configuration rather than replaying it as skipped, so the value is validated and persisted.
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
        - Interactive runs offer "Custom agent".
        - Non-interactive runs use `--custom-agent-*`.
        - The id must not be a registry agent id.
        - Provider/model setup is handled by the agent environment, not init flags.
    - Designated adapter (`[agent.adapter_override]`, registry agents only):
        - Declared with the `--adapter-override-*` flags or carried in an imported config; no interactive prompt lane.
        - Re-confirming the same agent preserves it.
        - An agent change, `--custom-agent-*`, `acps agent switch`, and `--adapter-override-clear` clear it.
        - Array targets never inherit it; a non-registry agent id is rejected.
    - While a managed-state endpoint override is stored, an agent apply is rejected when the target cannot carry the override:
        - A registry agent without `set_provider_base_url`.
        - Any custom agent.
        - A re-confirmed agent whose kept provider is the overridden one on a pair that refuses overrides (codex + `openai`, goose + a provider without a host setting, a provider row without a vendor `base_url` for the agent).
        - Clear the namespace's credential endpoint first; see [extensions.md](extensions.md#type-managed-state).
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
        - New selections default to `[agent.auto_update] enabled = true`, `frequency = "1d"`.
        - `--agent-update <on|off>` / `--agent-update-frequency <freq>` (or the interactive prompt) override it, so auto-update can be declined at init instead of only afterward via `acps agent update set --auto-off`.
        - Custom (non-registry) agents cannot auto-update, so `--agent-update on` is rejected.
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
    - For a Kilo config that authenticates through a provider-native credential (declared via `--agent-env-ref` or present in an imported config):
        - Seed the `KILO_API_KEY` declaration when missing and record an empty placeholder for it automatically.
        - The harness requires the variable present even with a non-Kilo provider, so no separate `secrets set` is needed.
        - `acps config import` and `acps agent set --model` apply the same rule outside init.
8. Agent install.
    - Registry agents install from the embedded catalog.
    - Custom agents install through `[agent.install]`.
    - Adapter-backed agents — including any registry agent with a designated adapter — install both harness and adapter unless the catalog marks the harness as adapter-provided.
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
    - System-scope actions run directly as root, or through `sudo -n` when the process is non-root and passwordless sudo is available.
    - Otherwise they are skipped with a warning listing the manual `sudo <shell> -c '…'` command per action and the `acps init --resume --deps-apply --deps-apply-yes` follow-up.
    - Privilege skips are recorded as `privilege_required` under `deps_apply`, surface in status/health, and do not fail init; genuine action failures still fail init.
    - `--deps-apply-async` records the step with disposition `background`. A resume adopts that run instead of launching another worker.
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
13. Provider, model, mode, effort, and session config options.
    - Supported registry agents:
        - Select or validate provider and required secret refs. A ref is satisfied by the flat secret store or by a structured catalog credential covering it (registry providers via their canonical mapping, custom providers via the configured `api_key_ref`); the provider picker's readiness labels and the resume-time idempotence check use the same rule.
        - Discover ACP-advertised model, mode, and reasoning-effort options from a provisional session. When the model lane changes the model, the harness config is re-provisioned and a second provisional session serves the effort and session-config-option lanes, since adapters advertise those per model. Effort values come from the agent's `thought_level` session config option; Codex with OpenRouter takes them from the provider catalog instead.
        - Interactive runs also prompt the select and boolean session config options the typed lanes do not own. An explicit answer persists under `[agent.config_options]`; a skip keeps the agent's advertised current value and writes no override.
        - The mode and effort lanes run only for agents the registry marks as supporting them; `--mode` or `--effort` against any other registry agent is rejected before discovery, as `--model` is.
        - Provider-backed agents need a provider (passed this run or already in config) before any lane runs, since the harness cannot be launched to advertise anything without one.
        - Interactive runs offer mode and effort selectors alongside the model selector.
        - A non-interactive run without `--mode`/`--effort` never enters that lane: it spawns nothing, prints nothing, and writes no value. The exception is an agent whose registry entry declares `default_mode` (kimi: `yolo`): the mode lane runs, and the default lands when the agent advertises it.
        - Explicit `--mode` and `--effort` are validated against the advertised values, and a rejection lists them. Codex with OpenRouter validates `--effort` against the provider catalog's reasoning-effort values for the configured model and pins the value in `~/.codex/config.toml`.
        - When mode and/or effort are the only active lanes and neither flag was passed, a provisional session that cannot be established is reported and skipped rather than failing init; an explicit `--mode`/`--effort` still fails loudly.
        - Discovery also requires a resolvable provider credential:
            - When hosted init has deferred the provider credential, discovery skips with a progress note; explicit `--model`, `--mode`, and `--effort` values are written without advertised-value validation.
        - Codex with a non-OpenAI provider lists models from the provider's live catalog (fetched during init and cached at `~/.config/acp-stack/provider-models.json`) instead of the adapter's advertised OpenAI presets.
        - When no catalog is available (custom provider or an offline fetch), the model step is skipped with a hint to rerun with `--model`.
        - Apply `--provider`, `--api-key-ref`, `--model`, `--mode`, `--effort`, and custom-provider flags.
        - A custom provider's id must not be one the mapped-provider registry already knows, including ids the registry maps for other harnesses: registry ids are reserved instance-wide, so codex with an Anthropic-compatible endpoint uses a distinct id such as `anthropic-1`.
        - Passing a registry id with `--custom-provider`, or selecting a registry id that has no key mapping for the chosen agent, is rejected with that remediation.
        - Kimi Code skips model discovery: `--model` is accepted as supplied, and without it init pins the selected provider lane's default (`kimi-for-coding` on the subscription lanes, `kimi-k3` on the Moonshot platform) unless config already has a model.
        - Provider selection follows the standard provider-backed rules; at runtime, a legacy config without `[agent.provider]` launches on the Kimi For Coding subscription lane.
    - Custom agents:
        - Skip provider/model/mode/effort discovery; interactive runs still prompt generic session config options from one provisional session.
        - A completed provisional session doubles as the ACP connection gate; otherwise one initialize-only gate runs when the launch command and cwd are present.
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

After the steps settle, init prints a summary: the config, state, secret-store, and age-key paths, and the auth status. Terminal runs also print one `ignored:` line per configured feature outside the probe's advertised capabilities. Hosted runs surface these only through the handoff payload's `ignored_features`.

## Key Handover

When no auth verifier rows exist, init generates two API keys and shows their plaintext values to you exactly once:

- Session key — session-driving and prompt-driving API calls.
- Admin key — secrets, config import, agent process control, and other elevated operations.

The handover prints the two values. Save them when shown:

- The values are never stored in plaintext, never returned through the API, and never reprinted on a later run.
- A re-run or `--resume` over existing verifier rows takes the preserved path and shows nothing.

Successful text-mode runs end with a next-step hint pointing at `acps serve` and `acps sessions new`, since init itself leaves no daemon running. The hint prints on the preserved path too, but not when a failed run renders keys through the drop guard.

`acps init --rotate-keys` regenerates both keys in place over existing verifier rows and shows the new plaintexts once, exactly like a fresh generation. The retired keys stop verifying immediately. A running daemon caches the verifier pair at startup, so restart it before the rotated keys are accepted. `acps reset --yes` remains the only other rotation path.

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
  "selection": {
    "provider": "openai",
    "model": "openai/gpt-5.5",
    "mode": "plan",
    "effort": null
  },
  "session_key": "acps_...",
  "admin_key": "acps_..."
}
```

- `selection` reports the agent selection the run settled into the written config: the provider id, the provider-native model id verbatim (prefix included, never joined to or split from the provider id), and the mode and effort. It appears only on the success payload and always carries all four keys; a lane that settled without writing a value reports an explicit null.
- `session_key` and `admin_key` appear only when that invocation freshly generated or rotated the keys. Rotated keys are reported under `generated_keys`.
- A later run without `--rotate-keys` preserves the verifier rows and reports `"preserved_keys": ["session", "admin"]` without reprinting either plaintext key.
- If init fails after fresh key generation, handoff mode emits the same shape with `"status": "failed"` and without `selection`, so automation can capture the one-time keys before retrying.

The payload carries an `ignored_features` array (omitted when empty) listing configured features outside the capability probe's advertised set: `[{"feature": "mcp.server", "value": "linear", "capability": "mcpCapabilities.http", "reason": "..."}]`.

A `--deps-apply-async` run adds `deps_apply_run_id` (omitted otherwise), identifying the run to poll through `GET /v1/deps/apply/runs/{apply_run_id}` once the daemon is available.

## Hosted Streaming Init

`acps init serve` runs a bootstrap-only HTTP/WebSocket server for hosted init. The hosted backend connects to the instance; the web UI reaches the instance only through the hosted backend. Bootstrap auth uses a bearer token from `ACP_STACK_INIT_TOKEN`, `--token-env`, or `--token-file`.

The hosted flow follows the same init steps as interactive `acps init`, but streams only the bootstrap prompts needed for:

- Agent selection.
- Provider selection.
- Required secret collection.
- Custom-provider fields.
- Model, mode, and effort selection.
- Select and boolean ACP session config options not owned by those typed settings.
- The post-install MCP setup.
- The simple confirmations on that path.

### Stream Rules

- Every streamed prompt carries a machine-readable `kind` and, for selections, stable option values, so a client routes and answers by id rather than by prompt text.
- After credentials are available, the provisional `session/new` response drives the typed model/mode/effort prompts and generic `config_option` prompts. Explicit generic answers persist under `[agent.config_options]`; a skipped option keeps the agent's advertised current value.
- Environment configuration (skills, dependencies, browser-use, data sources) and the acp-stack/agent auto-update policies (`stack_update`/`stack_update_frequency`, `agent_update`/`agent_update_frequency`) are declared up-front in the session-create request instead of being streamed. These wizard prompts remain outside the streamed set, and the request fields map onto the same init arguments the wizard would produce.
- Secret collection covers the refs those declarations name: MCP env/header refs (whole-value refs and refs named inside `${}` templates alike) and S3 data-source key refs missing from the store are requested as `password` inputs.
- An unanswered ref skips without failing init and surfaces later through MCP health or workspace materialization.
- Provider credential deferral follows the [`defer_provider_credentials` request contract](api/endpoints.md). While the credential is pending, prepared-config validation skips its unresolved provider refs.
- Normal `acps init` keeps its existing terminal behavior; hosted prompts outside the streamed set use the same skip/default behavior as non-interactive init unless supplied through initial args.
- The post-install MCP configuration step streams its prompts only when the start request declared no MCP server. Declaring servers up front still wins and skips the wizard outright.
- MCP declarations the installed agent's capabilities do not cover are reported only through the result frame's `ignored_features`, never through progress frames.
- Prompt answers arrive over the WebSocket `input` frame or the REST twin `POST /v1/init/sessions/{id}/input`, interchangeably. The bootstrap server also mounts `GET /v1/models` (with `?target_id=` target selection) so a backend renders pickers while the session runs.

### Extension Declarations And In-Stream Credential Deposit

The session-create request may carry an `extensions` map, staged into a freshly-created starter config before any tracked step runs:

- A managed-state declaration names the namespace the platform later pushes credentials into. A network-provider declaration routes every sandboxed init phase through the egress provider from the start, since the declaration lands before install, probe, and discovery run.
- A network-provider declaration pairs with `sandbox_mask_paths`: absolute paths (the provider's config and state dirs) unioned into the starter config's `[workspace.sandbox].mask_paths`, so the sandboxed agent cannot read them from the first spawn. Blank and relative entries are rejected.
- Declarations apply only to a starter config; a request carrying `extensions` or `sandbox_mask_paths` against an existing config is rejected. The exception is `resume`: the recorded run's original staging stands and re-declarations are ignored, matching `data_sources` and `deps`.

While the session runs, the platform pushes the sealed provider credential through `POST /v1/init/credential`:

- The body carries flat-store secrets beside a managed-state apply (`namespace` plus the admin-tier apply body verbatim), committed under one lock. Model discovery reads the config and secret store fresh from disk, and the provider lane reads through the serve process's shared store handle, so the deposit is visible on their next read.
- A ref that soft-passed under `defer_provider_credentials` resolves once the deposit lands, and the provider lane switches to live resolution without restarting the session.
- An identical replay at the same revision is a noop, so reconnects may re-deposit safely. A deposit before the starter config exists is rejected with `init.config_not_ready` and retried after staging.

### Signals

Alongside the prompt stream, the session reports structured readiness through `signal` events:

- Each signal is one raw fact: a step starting or finishing, or one of the ten categories (agent, provider, model, mode, effort, workspace, native config, MCP, skills, dependencies) becoming applicable, settled, or failed.
- The instance forwards the facts and does not derive a rendered view; the client folds the stream into one.
- The whole signal stream so far rides the `hello` frame and the status response, so a client that connects late folds the same input without parsing progress text.
- The signal shapes, the fold's category vocabulary, and its status values are in [api.md](api/api.md#bootstrap-init-api).

### Resume And Custom Agents

A hosted session can also continue a crashed run and bring its own agent:

- `resume`/`fresh` in the start request behave like `--resume`/`--fresh`.
- The `custom_agent_*` fields declare an escape-hatch agent the way the `--custom-agent-*` flags do.
- Custom-agent prompts are not streamed, so that declaration must be complete in the request.
- The instance emits `category_applicability` signals marking provider, model, mode, effort, and skills not applicable, and the client fold renders them so, since a non-registry agent configures those through its own environment.

### Native Config Upload

A fresh registry-agent session may include one in-memory `native_config` upload:

- The server inspects it before durable init work and sends a `native_config_review` input containing only the redacted manifest.
- The response selects compatible managed field ids and acknowledges executable unmanaged categories for that revision.
- After agent installation, init commits the journaled semantic replacement before provider/model discovery and before the first persistent agent start, so onboarding does not restart an agent.
- The source document is excluded from recorded init arguments, events, progress, and handoff metadata; the final handoff may include only the sanitized `native_config_import` operation.

While a result is awaiting acknowledgement, `POST /v1/init/sessions/{id}/native-config/cancel` with `{ "operation_id", "revision" }` rolls back an applied onboarding import:

- The server restores the journaled pre-import snapshots and returns the operation with status `cancelled`.
- The backend uses this as a compensating action when it cannot validate or persist the reported operation; retries are idempotent.
- Unlike the runtime cancel route, this rollback does not gate on applied-file digests, because later init steps legitimately rewrite the canonical config after the onboarding apply.

### Final Result Delivery

- Delivery uses the same handoff payload shape as `--handoff-json`.
- Hosted init always rotates existing keys, and the plaintext session/admin keys travel only in the WebSocket `result` frame, which always carries `session_key` and `admin_key`.
- Status and event replay report only non-secret state.
- The backend must send `ack_result` after storing or forwarding the keys.
- After acknowledgement, the in-memory result is cleared, the session closes, and the bootstrap server exits successfully.

### Failure Paths

- A failure after key handover still delivers a `"status": "failed"` result frame through the same result/ack path.
- A failure before key handover parks instead of exiting immediately:
    - The session turns `errored`.
    - The typed error stays retrievable through the status route, the reconnect `hello`, and the `replay_error` frame.
    - The backend sends `ack_error` to release the server, which then exits non-zero.
- An unacknowledged error expires after a 2-minute grace (reason `error_ack_timeout`) even while a WebSocket is connected, and the server still exits non-zero.

### Lifetime Bounds

The server bounds its own lifetime so even an abandoned bootstrap eventually frees the bind port:

- When no WebSocket client is connected and no API call has arrived for `--idle-timeout` (default `15m`; `0s` disables), the session is cancelled with reason `idle_timeout`.
- `--max-lifetime` (disabled by default) does the same with reason `max_lifetime` once the absolute cap elapses, regardless of activity.
- A WebSocket disconnect restarts the idle clock, so a dropped backend gets the full timeout to reconnect and acknowledge.
- Expiry also fires from `completed_awaiting_ack`: the stored unacknowledged result is discarded and zeroized, and the process exits non-zero.
- A parked `errored` session has its own 2-minute `error_ack_timeout` grace that runs regardless of the idle setting and of connected clients.
- Reaching either limit before any session was created also exits non-zero. The pre-session idle clock runs from the last authenticated API call, not just server start.
- When a session turns terminal, the server closes any attached WebSocket after forwarding the final event, so a hung client cannot hold the process past `--max-lifetime`.
- A session status snapshot includes `last_activity_age_secs` — the idle time leading up to that status request, before the request itself counts as activity — so the hosting backend can make its own reap-vs-wait decisions.

## Testflight

After config and secrets are present, init can run a testflight. It starts the configured agent and sends a minimal real prompt to verify the connection end to end:

- Session creation, prompt completion, streamed updates, and a terminal prompt state.
- At least one filesystem-visible tool action when the agent supports tools.

Testflight is opt-in because it may consume provider credits:

- Interactive runs prompt with a credit warning before running.
- `--testflight` runs it without prompting; `--skip-testflight` skips it.
- Non-interactive runs skip testflight unless `--testflight` is passed.
- A provider credential pending under hosted deferral skips testflight, naming the provider and ref; an explicit `--testflight` fails with the same remediation.
- A hosted run whose backend answers the credit-warning prompt with `value: false` and `deferred: true` (see [api.md](api/api.md)) reports `testflight: deferred (runs after setup)` rather than a decline: the backend intends to run the test itself once setup completes.

Testflight hard-fails on paths outside its supported set: browser-OAuth agents, private Drive/Dropbox links, non-archive cloud folders, unsafe archives, and missing required secrets. It also fails when an agent appears active but the configured timeout elapses with progress and terminal state both still pending.

## Related

- [cli.md](cli/cli.md#initialization) — the `acps init` flag reference.
- [config.md](config.md) — the config schema init writes.
- [runtime.md](runtime.md) — the resumable step machine and workspace materialization.
- [security.md](security.md) — key generation and the admin-key policy.
- [agents/](agents/) — per-agent install, launch, and auth setup.
