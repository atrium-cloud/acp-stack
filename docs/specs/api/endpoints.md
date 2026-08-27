# Endpoint Reference

All public HTTP routes are versioned under `/v1`. Clients authenticate with a bearer key:

```http
Authorization: Bearer <key>
```

The tier names (`session`, `admin`, `bootstrap`), the response envelope, and the error-code model are defined in [api.md](api.md). Each entry below uses the same fields:

- Tier — which key authorizes the route.
- Request — query parameters and body shape.
- Response — the `data` payload of the success envelope.
- Errors — route-specific typed error codes.
- Notes — behavioral contract facts.

## Local Socket Exposure

The local Unix-socket router always serves selected low-risk daemon-backed routes keylessly for local `acps` commands:

- `GET /v1/sessions`
- `GET /v1/sessions/-/status`
- metrics summary
- WebSocket summaries
- the security diagnostic

Session-tier HTTP routes are also mounted on the local socket and serve only while `[local].session_auth = "keyless"` is active, returning 404 otherwise. The local socket omits admin-tier routes, auth rotation, config import, secret mutation, dependency apply, WebSocket disconnects, and WebSocket upgrades.

## Bootstrap Init

`acps init serve` exposes the routes in this section plus the session-tier `GET /v1/models` (below), served here on the `bootstrap` tier; this mode omits the other normal session/admin `/v1` routes. Calls use exactly one `Authorization: Bearer <bootstrap-token>` header; the token comes from process input alone.

### `POST /v1/init/sessions`

- Tier: `bootstrap`
- Request: starts one active init session. Accepts optional initial agent/provider/model/workspace args, environment configuration declarations, and an in-memory native config upload. Unknown request fields are rejected. Body fields, grouped:
    - Agent/provider/model/mode/effort: `agent`, `provider`, `api_key_ref`, `model`, `mode`, `effort`, `custom_provider`, `provider_name`, `base_url`, `provider_api`, `model_name`, `context`, `output_max_tokens`, `skip_testflight`, `testflight`, `native_config` (`{ "filename", "content" }`).
        - `mode` is the initial session mode id, validated against the agent's ACP-advertised `mode` values by the same provisional session that discovers models. Declaring it skips the streamed mode picker.
        - `effort` is the initial reasoning-effort value, validated the same way against the agent's advertised `thought_level` values. Declaring it skips the streamed effort picker.
        - `api_key_ref` and `custom_provider` each require `provider`. `provider_name`, `base_url`, `provider_api`, `model_name`, `context`, and `output_max_tokens` each require `custom_provider: true`.
    - Custom agent (the escape-hatch agent, mirroring the `--custom-agent-*` flags): `custom_agent_id`, `custom_agent_name`, `custom_agent_command`, `custom_agent_args` (array), `custom_agent_install`, `custom_agent_creates`.
        - Custom-agent prompts are never streamed, so the whole spec must arrive here.
        - Every other field in this group requires `custom_agent_id`.
        - `custom_agent_id` requires non-blank `custom_agent_command` and `custom_agent_install`, and conflicts with `agent`, `provider`, `model`, `mode`, `effort`, and `custom_provider: true`.
        - Reserved and registry-colliding ids are rejected later, in-session, where the registry is in hand.
    - Run selection: `resume` and `fresh` booleans, matching `--resume` and `--fresh`; they conflict with each other.
        - `resume` continues the most recent unfinished or failed run instead of starting another. Recorded arguments replay, and any field set in this request layers on top like an explicit flag.
        - Hosted init rotates keys on every run, including a resumed one.
    - Workspace: `workspace_root`, `workspace_uploads`, `runtime_user`, `sandbox`, `code_from` (array of git URLs), `data_from` (array of local paths or https archive URLs).
    - Environment configuration (mirrors the `acps init` flag and wizard surface). These replace the interactive environment wizard, which is never streamed to hosted clients. The one exception is the post-install MCP step, whose prompts stream when this request declared no MCP server.
        - `mcp_preset`: array of preset ids (`linear`).
        - `mcp_stdio`: array of `{ "name", "command", "args": [...], "env": [...] }`. Each `env` entry is a bare secret ref name exported into the server's environment, or a `VAR=template` entry whose template interpolates `${SECRET_REF}` (rules in [config.md](../config.md)).
        - `mcp_http`: array of `{ "name", "url", "headers": [{ "name", "value_ref"?, "value"? }] }`. Each header sets exactly one of `value_ref` (whole-value secret ref) or `value` (a `${SECRET_REF}`-interpolated template). URLs must be https, or http to a loopback host (a local relay endpoint).
        - `skills_source` + `skills`: explicit skill selection, same semantics as `--skills-source`/`--skills`; both must be declared together. `essential_skills`: boolean, conflicts with the explicit pair. An unsatisfiable skills declaration (e.g. the selected agent has no Agent Skills install directory) fails the init session.
        - `deps` and `deps_system`: arrays of `{ "name", "shell" }` install records (user/system scope). `deps_apply` + `deps_apply_yes`: booleans, must be set together (the interactive apply confirmation is never streamed). When set, init runs the declared install actions; otherwise dependencies are declared in config but not installed. `deps_apply_async` requires `deps_apply` and makes the dependency step report disposition `background`. `standard_agent_work_deps` and `browser_use`: booleans enabling the standard dependency bundle and the browser-use profile.
        - `data_sources`: array of tagged records — `{ "type": "local", "path" }`, `{ "type": "https", "url", "expected_sha256"?, "max_download_bytes"?, "max_extracted_bytes"? }`, or `{ "type": "s3", "bucket", "region", "prefix"?, "access_key_ref", "secret_key_ref" }` — each with an optional `name`.
    - Extensions: `extensions`, a map of extension name to declaration table, e.g. `{ "network-egress": { "type": "network-provider", "provider": [...] } }`.
        - Declarations stage into a freshly-created starter config before any tracked step runs, so a network-provider declaration routes every sandboxed init phase through the egress provider from the start.
        - Applies only when creating a starter config; a request carrying `extensions` against an existing config is rejected. The exception is `resume`: a resumed run keeps the recorded run's original staging, and re-declared `extensions` are ignored, matching `data_sources` and `deps`.
        - Semantic validation (name shape, per-type field discipline, the network-provider/unshare pairing) runs in-session with the rest of config validation, matching the deferred-validation note below.
        - `sandbox_mask_paths`: array of absolute paths unioned into the starter config's `[workspace.sandbox].mask_paths`. A network-provider declared here needs its egress config and state dirs masked from the first sandboxed spawn, so the caller declares them alongside. Entries must be non-blank absolute paths, validated in-session like the extension declarations; duplicates collapse. Applies only when creating a starter config, with the same rejection discipline as `extensions`.
    - Update policies (mirror the `--stack-update`/`--agent-update` flags; declared up-front, never streamed):
        - `stack_update` (`on` | `security` | `off`) with optional `stack_update_frequency` (day/week units, e.g. `1d`, `3w`).
        - `agent_update` (`on` | `off`) with optional `agent_update_frequency` (hour/day/week units, e.g. `12h`, `1d`).
        - Each `*_frequency` requires its policy. Omitted policies leave the config schema defaults intact.
        - `agent_update` is honored only for managed registry agents. `agent_update: "on"` against a custom agent fails the session.
    - `defer_provider_credentials` (boolean, default `false`): declares that the caller will push the configured provider's credential through the managed-state extension after init. A missing ref the push can deliver — a custom provider's api-key ref, or a mapped key-based provider's api-key and companion env vars under the names the agent reads — is not prompted and soft-passes. A ref the push cannot deliver stays required and fails the session: a noncanonical api-key alias, a `VAR=template` inner ref, and an agent-native-auth provider's refs. Without the declaration, a missing provider ref fails the session.
- Response: `{ "session_id": "...", "status": "running" }` in the standard success envelope. `status` is one of `running`, `waiting_for_input`, `completed_awaiting_ack`, `errored`, `cancelled`, or `closed`. The same set backs the status route and the cancel route's `{session_id, status}` body.
- Errors:
    - `409 init.session_active` — another session is running or awaiting result acknowledgement. Also returned while a failure is parked (see lifecycle below).
    - `400` — a cross-field rule is violated. The error names the offending field and never echoes its value.
    - `400` — MCP secret-value position violations: env entries and header `value_ref`/`value` carrying pasted-credential shapes (rejected without echoing the value), ref-name or template syntax failures, or headers violating the exactly-one rule.
    - `400` — a `*_frequency` with no matching policy.
- Notes:
    - The route validates request shape, the cross-field rules, and the MCP secret-value positions in full at the boundary. Remaining semantic validation of field values (MCP URL scheme rules, data-source paths, and the enumerated values of `stack_update`, `agent_update`, `sandbox`, and `provider_api`) happens in-session. A declaration invalid only in those ways returns `200` with `"status": "running"` and then fails the init session.
    - Secret values referenced by these declarations (MCP `env`/`value_ref` entries, refs inside `${}` templates, S3 key refs) are never carried in the request body. Init collects any refs missing from the secret store over the prompt stream as `password` inputs with `required: false`.
    - Answering a secret-ref prompt with `null` skips the ref without failing the session. A skipped MCP secret later surfaces through runtime MCP health. A skipped S3 key ref fails workspace materialization.
    - Provider key refs follow the `defer_provider_credentials` rule above. Otherwise their prompts stream `required: true`; a `null` answer is accepted, but a still-unresolved ref fails the session.
    - The values never appear in status or event replay.
    - Environment declarations the installed agent's capabilities do not cover do not fail the session. They are written to config, skipped at runtime, and reported only through the result payload's `ignored_features` (see [init.md](../init.md#platform-handoff-json)), never through `progress` frames.

### `GET /v1/init/sessions/{id}`

- Tier: `bootstrap`
- Request: none.
- Response: non-secret status, the `signals` replay, pending input, recent progress, `last_activity_age_secs`, and `completed_awaiting_ack` when a result exists.
    - Pending input entries carry `request_id`, `kind`, `style`, `prompt`, `required`, optional `default`, and per-option `index`, `value`, `label`, and `hint`.
    - `kind` is the machine-readable prompt identity (`agent`, `provider_id`, `model`, `mode`, `effort`, `mcp_transport`, `secret_ref_value`, and so on) and is the field a client routes on. `style` remains the rendering hint.
    - Option `value` is a stable id that survives display rewording, so answers may address a choice as `{"value": "<id>"}` in addition to the index, label, and `null` forms. An unknown value is rejected as an invalid parameter.
    - A native upload produces `style: "native_config_review"` plus the redacted `inspection`. Its client response value is the revision-bound selection object used by the normal import contract.
    - `last_activity_age_secs` is the idle time leading up to that request, measured before the request itself counts as activity. It supports backend-side reap decisions.
- Errors: none route-specific.
- Notes:
    - Status never includes plaintext session/admin keys or secret input values. The server does not replay keys through status or generic events.
    - On a parked failure, the typed `error` payload is available through this route.

### `GET /v1/init/sessions/{id}/events?after_seq=N`

- Tier: `bootstrap`
- Request: `after_seq` query parameter.
- Response: replays non-secret progress, signal, and input lifecycle events. Entries are the same seq-bearing frames a live client receives. Bounded to recent history and may not reach the oldest signals; the authoritative full replay is the `signals` field of the status body and `hello`.
- Errors: none route-specific.

### `POST /v1/init/sessions/{id}/input`

- Tier: `bootstrap`
- Request: `{ "request_id": "...", "value": <any>, "deferred": false }`. The REST twin of the WebSocket `input` frame, with the same fields, defaults, and answer semantics, parsed by the same prompt-driver logic. Unknown fields are ignored, matching the frame, so a client may post a socket frame verbatim (including its `type`).
- Response: `{ "request_id": "..." }` in the standard success envelope. The `input_accepted` event still reaches subscribed sockets.
- Errors:
    - `404 init.session_not_found` — no such init session.
    - `409 init.input_rejected` — no pending input, or a stale `request_id`. The HTTP equivalent of the socket's `init.input_rejected` error frame.
- Notes: a backend that polls over REST answers prompts here instead of holding a socket open; the two transports are interchangeable.

### `GET /v1/init/sessions/{id}/ws`

- Tier: `bootstrap`
- Request: WebSocket upgrade.
- Response: the hosted init WebSocket stream.
    - Server frames: `hello`, `progress`, `signal`, `input_required`, `input_accepted`, `result`, `error`.
    - Client frames: `input`, `cancel`, `replay_result`, `ack_result`, `replay_error`, `ack_error`.
- Errors: none route-specific.
- Notes:
    - Client `input` frames must include the active `request_id`; stale input is rejected. Unknown fields on a client frame are ignored rather than rejected.
    - One optional input field is defined: an `input` frame answering the `testflight_confirm` prompt may carry `deferred: true` beside `value: false`. It tells init the answer is a hosting backend that will run the testflight itself after setup rather than an operator declining it.
    - The testflight step then reports `testflight: deferred (runs after setup)` and records the `SkipDeferred` decision instead of `SkipDeclined`. The flag is ignored on every other prompt and on an accepting answer.
    - The final `result` frame carries the platform handoff payload and always includes plaintext `session_key` and `admin_key`. Hosted init generates them on a fresh instance and rotates them over pre-existing state, so every result carries working keys.
    - On an `init.ws_lagged` frame the server re-sends `hello`; the client re-folds it and continues.
    - A client must ignore unrecognized signal types, applicability `source` values, and prompt kinds rather than failing, so the stream can gain variants without breaking existing clients.

#### Signal Frames

Each `signal` frame reports one raw fact the wizard observed: a step starting or finishing, or a category becoming applicable, settled, or failed. A client renders progress from structure instead of parsing `progress` text.

- Signals are seq-bearing events. They appear in event replay (`after_seq`) alongside `progress`, one event per fact and no dedup.
- The instance forwards these facts raw; the client folds the stream into a rendered category view.
- `hello` and the status response body carry a `signals` array — the whole stream so far, in order. A client that connects late, or reconnects after the bounded event history evicted early events, folds the same input as a full-stream client.
- The signal set is bounded by init's structure (a fixed set of steps and nine categories), so the replay is safe to carry in full.
- A client re-folds from the `hello`/status replay and drops any live frame whose `seq` is at or below the replay's `last_seq`.

```json
{ "signal": "step_started", "step": "provider_configure", "seq": 12, "session_id": "...", "type": "signal" }
{ "signal": "step_finished", "step": "provider_configure", "disposition": "executed", "error_code": "init.provider_write_failed", "seq": 13, "session_id": "...", "type": "signal" }
{ "signal": "category_applicability", "category": "mcp", "applicable": false, "source": "probe", "reason": "agent does not advertise MCP support", "seq": 14, "session_id": "...", "type": "signal" }
{ "signal": "category_settled", "category": "agent", "value": "opencode", "seq": 15, "session_id": "...", "type": "signal" }
{ "signal": "category_provisionally_settled", "category": "mode", "value": "default", "seq": 16, "session_id": "...", "type": "signal" }
{ "signal": "category_failed", "category": "skills", "code": "init.skills_install_failed", "seq": 17, "session_id": "...", "type": "signal" }
```

- `error_code` is present only on a failed step. `reason` is present only on an inapplicable verdict. A `category_settled` `value` is `null` for a settlement that wrote nothing.
- `source` is one of `args`, `registry`, `probe`, `discovery`, `discovery_unavailable`.
- Settlement values are ids and secret ref names only (the provider settles with the configured provider id, not the key behind it), never secret values.

#### Signal Fold Model

The fold produces all ten categories in this order: `agent`, `provider`, `model`, `mode`, `effort`, `workspace`, `native_config`, `mcp`, `skills`, `deps`. Each takes a `status`, resolved in the precedence below. A category qualifying for more than one takes the first that matches.

- `failed` — with the typed error `code` that broke the lane. A lane that broke did run, so failure outranks a `not_applicable` verdict that arrived before it.
- `not_applicable` — this run has no such lane (`registry` says the agent takes no provider, a `probe` found no MCP support, `discovery` found no modes, the operator skipped workspace init). A `reason` names what ruled the lane out; it is the only status that carries one. Authority is ranked: a `probe`, `discovery`, or `discovery_unavailable` verdict is the installed harness talking, so a later `registry` claim never revives the lane.
- `awaiting_input` — the pending prompt belongs to this category. The client derives this from `pending_input` by mapping the prompt's `kind` to a category:
    - `agent` → `agent`
    - `provider_id`, `provider_name`, `base_url`, `api_key_ref`, `provider_api_key_value` → `provider`
    - `model` → `model`; `mode` → `mode`; `effort` → `effort`
    - `native_config_review` → `native_config`
    - the MCP prompts (`mcp_add`, `mcp_transport`, `mcp_row_action`, the `mcp_stdio_*` and `mcp_http_*` kinds) → `mcp`
    - every other kind — secret-ref, testflight, config-source, custom-agent, skills, dependency, data-source, and update-policy prompts — maps to no category and leaves nothing awaiting
    - A prompt awaits from its `input_required` until the matching `input_accepted`. At most one category holds `awaiting_input`, since there is one pending input at a time.
- `settled` — done, with an optional `value` naming what was written.
    - A settlement (`category_settled`) and a failure are this run's own evidence and are never withdrawn as inapplicable. A settled lane still moves to `failed` if the step behind it breaks afterwards.
    - A `category_provisionally_settled` value — a value read off configuration that predates the run, what a resumed or fully declared run reports for its provider, model, and mode lanes — is withdrawn, value and all, when a `probe` or `discovery` verdict finds the installed agent no longer has the lane. It is never withdrawn merely because that live check could not be made (`discovery_unavailable`).
- `blocked` — waiting on the category named in `blocked_on`. `provider` waits on `agent`, `model` on `provider`, `mode` and `effort` on `model`, and both `mcp` and `skills` on `agent`. `workspace`, `native_config`, and `deps` wait on nothing.
- `ready` — applicable, unblocked, not yet settled.

The client also folds `current_step` from the `step_started`/`step_finished` stream: the last step named, using the step-kind vocabulary (`agent_install`, `capability_probe`, `mcp_configure`, `provider_configure`, and so on), `null` before the first step.

- Steps with no category of their own move no category.
- A `step_finished` with no `error_code` settles the step's category if nothing else did. The `init_complete` step settling successfully sweeps every still-open applicable lane to `settled`. A failed final step runs no sweep.
- `step_finished.disposition` is `executed`, `skipped`, or `background`. `background` means the step launched work that continues after the step returned.
- Cross-cutting prompts that belong to no category (`secret_ref_value`, `testflight_confirm`, and the other setup kinds) leave nothing awaiting input.
- A failing step emits `step_finished` with an `error_code` before the terminal `error` frame. The fold badges the step's category `failed`, unless another lane already claimed that same code (`provider_configure` covers the provider, model, mode, and effort lanes, so whichever broke badges itself) or the lane is one this run does not have. Those two guards keep a step error from inventing or duplicating a badge; there the `error` frame carries the failure alone.
- A specific lane may also fail on its own through a `category_failed` signal emitted by the code that owns it (a provider write, say), which the fold applies unconditionally.
- A failure that owns no running step emits no category-bearing signal at all.
- The frontier freezes with the session: once it is cancelled, closed, errored, or awaiting result acknowledgement, no later `signal` is recorded and the replay in `hello` and the status route stops growing. The terminal frame is the last word on what the run settled.

#### Init Session Lifecycle

- After `result`, the session remains `completed_awaiting_ack`. If the WebSocket drops before acknowledgement, the backend reconnects and sends `replay_result`. `ack_result` is terminal: the server clears the in-memory handoff payload, closes the session, and exits successfully.
- A failure after key handover still delivers a `result` frame (with `"status": "failed"` and any freshly generated keys) through the normal result/ack path.
- A failure with no result payload to deliver — before key handover completed — parks instead: the session enters `errored` and the server stays up so the backend can learn the typed failure instead of a dead port.
    - The `error` payload is available through the status route, the reconnect `hello` frame, and `replay_error`. `ack_error` releases the server, which exits non-zero.
    - `cancel` is a no-op on a parked failure, like on an un-acked result.
    - If no `ack_error` arrives within a 2-minute grace (enforced regardless of `--idle-timeout` and of connected WebSockets), the server expires the error with reason `error_ack_timeout` and exits non-zero on its own.
- The server also self-terminates abandoned sessions:
    - After `--idle-timeout` (default `15m`) with no connected WebSocket and no API activity, or once `--max-lifetime` elapses, the session is cancelled (reason `idle_timeout`/`max_lifetime`).
    - Any un-acknowledged result is discarded, attached WebSockets are closed server-side after the final event, and the process exits non-zero.
    - This applies even when a limit is reached before any session was created; the pre-session idle clock runs from the last authenticated API call.
    - A WebSocket disconnect restarts the idle clock, leaving the full timeout for the documented reconnect-and-`replay_result` flow.

### `POST /v1/init/sessions/{id}/cancel`

- Tier: `bootstrap`
- Request: none.
- Response: the post-cancel `{session_id, status}`.
- Errors: none route-specific.
- Notes: cancels the active session with reason `backend_cancel`. A no-op on a session already `closed`, `cancelled`, `errored`, or `completed_awaiting_ack`.

### `POST /v1/init/sessions/{id}/native-config/cancel`

- Tier: `bootstrap`
- Request: `{operation_id, revision}`.
- Response: standard envelope.
- Errors: `409 init.result_unavailable` — unless a result is awaiting acknowledgement (see [init.md](../init.md)).
- Notes: cancels a queued native-config import or rolls back the latest applied one.

### `POST /v1/init/credential`

- Tier: `bootstrap`
- Request: `{ "secrets": [{ "name", "value" }], "namespace", "apply": { "schema_version", "revision", "desired" } }`. Unknown fields are rejected.
    - `secrets`: flat-store secrets written before the managed apply resolves, so a `source_refs` entry in `desired` may reference a name this same body deposits. At most 16 entries, each value at most 16 KiB.
    - `namespace`: the managed-state extension namespace the apply targets. It must resolve to a declared `type = "managed-state"` instance in the runtime config, so the platform declares that extension through the `extensions` field of the session request.
    - `apply`: the admin-tier managed-state apply body verbatim, as accepted by `POST /v1/admin/extensions/{name}/apply`; the `selection` key semantics carry over unchanged.
- Response: `{ "secrets_written", "applied_revision", "outcome" }` in the standard success envelope. `outcome` is `applied`, `cleared`, or `noop`.
- Errors:
    - `409 init.config_not_ready`: no runtime config exists yet; retry once the session has staged one.
    - `404 extensions.not_found`: `namespace` does not resolve to a declared `type = "managed-state"` instance.
    - `409 extensions.revision_conflict`: revision-ordering conflict against the namespace watermark.
    - `400 extensions.state_ownership`: the desired state touches entries owned by the operator or another namespace.
    - `400 request.invalid_param`: ref-name shape, count, size, or duplicate-name violations; the error never echoes a rejected value.
- Notes:
    - Secret writes and the managed apply commit under one lock, so a fresh-from-disk store read (model discovery, `/v1/models`) and the serve process's shared in-memory handle (the init provider lane) each observe both or neither. An identical replay at the same revision is a `noop`.
    - Values are opaque to acp-stack, stored verbatim, and never replayed through status, events, or errors.
    - The deposit lands while an init session runs: a previously soft-passed provider ref resolves on the next read, switching the provider lane to live resolution without restarting the session.
    - Deposits are accepted whenever the bootstrap server runs and a runtime config exists, including before a session starts or after one completes, under the same bootstrap token.

## Config And Secrets

The API withholds secret values from every response. Auth keys live outside the secret store.

### `GET /v1/config/export`

- Tier: `session`
- Request: none.
- Response: current canonical TOML with secret refs only.

### `POST /v1/config/validate`

- Tier: `session`
- Request: raw TOML.
- Response: validation result. Nothing is written.

### `POST /v1/config/import`

- Tier: `admin`
- Request: canonical TOML; supports `dry_run=true`.
- Response: untyped import response (not covered by the JSON Schema).
- Notes: validates and writes canonical TOML.

### `GET /v1/secrets`

- Tier: `admin`
- Request: none.
- Response: secret names only.

### `POST /v1/secrets`

- Tier: `admin`
- Request: a secret name and value.
- Response: standard envelope.
- Notes: stores or replaces a secret value.

### `DELETE /v1/secrets/{name}`

- Tier: `admin`
- Request: none.
- Response: standard envelope.
- Notes: deletes a secret.

### `POST /v1/auth/session-key/regenerate`

- Tier: `admin`
- Request: none.
- Response: the new plaintext session key, returned once.
- Notes: replaces the session verifier.

### `PUT /v1/auth/local-session-access`

- Tier: `admin`
- Request: the new `[local].session_auth` value.
- Response: standard envelope.
- Notes: sets `[local].session_auth` and applies it to the running daemon.

### `POST /v1/admin/extensions/{name}/apply`

- Tier: `admin`
- Request: `{schema_version, revision, desired}`.
- Response: standard envelope.
- Errors:
    - `404 extensions.not_found` — `{name}` does not resolve to a declared `type = "managed-state"` instance.
    - `409 extensions.revision_conflict` — revision-ordering conflict.
    - `400 extensions.state_ownership` — provenance refusal.
    - `400 request.invalid_param` — a provider id that is neither mapped nor configured as a custom provider.
- Notes: the managed-state extension seam. Applies one managed-state registry revision to the named extension namespace. Full contract in [extensions.md](../extensions.md).

## Agent And Providers

### `POST /v1/agent/install`

- Tier: `admin`
- Request: none.
- Response: the `install` object with `outcome` of `installed` or `already_present`. The same object is embedded in the `POST /v1/agent/switch` response and is the body of `POST /v1/array/targets/{id}/install`.
- Notes: installs the configured supported agent. This is the live-progress surface pair for `GET /v1/installer/runs?active=true&agent=<id>`; installs can run for minutes.

### `POST /v1/agent/update`

- Tier: `admin`
- Request: optional `{ "force": bool }` (default `false`). `force` reinstalls even when the resolved target version matches the installed one.
- Response: the updater report `{ "agent_id", "updated", "skipped", "reason"?, "steps": [{ "step", "status", "method"?, "installed"?, "latest"?, "message"? }] }`.
    - Step `status` is `updated`, `up_to_date`, `skipped`, or `failed`.
    - `installed` is the version before the update and `latest` the resolved target (github/npm only — apt and native updates have no capturable version).
    - `up_to_date` is a first-class no-op success.
- Errors: only infrastructure errors (unreadable registry, state open failure) produce an error envelope. Failed steps still return `200` with per-step `failed` status and `message`.
- Notes:
    - Runs the same managed update path as the auto-update timer, synchronously, for the configured agent (harness, plus adapter when the registry pairs one).
    - Works with `[agent.auto_update]` disabled or absent, which is the intended mode for platforms that own update scheduling themselves.
    - A running (or starting/stopping/updating) agent is never touched: the route returns `200` with `skipped: true` and reason `agent is running`, including for a second update request arriving while one is in flight. Callers may retry safely.
    - A non-registry (escape-hatch) agent likewise returns `200` with `skipped: true`.
    - A `harness_version` pin constrains the update target the same way it constrains install: the pinned GitHub Release tag is used instead of the latest release (harness component, github path only). A pinned agent already at its pin reports `up_to_date`.
    - Each run records `agent.update.started` plus a terminal `agent.update.finished`/`agent.update.skipped`/`agent.update.failed` lifecycle event, payload-tagged with `"trigger": "api"` to distinguish it from the timer's runs. These surface in `GET /v1/agent/status` `lifecycle_events`.

### `POST /v1/agent/start`

- Tier: `admin`
- Request: none.
- Response: standard envelope.
- Notes: starts the supervised agent process. Uses the current `[agent]` config and the shared resolved environment, including selected provider credential bundles.

### `POST /v1/agent/stop`

- Tier: `admin`
- Request: none.
- Response: standard envelope.
- Notes: stops the supervised agent process. The loaded provider snapshot is cleared on stop or exit.

### `POST /v1/agent/restart`

- Tier: `admin`
- Request: query parameters `require_idle=true` and `auto=true`.
- Response: standard envelope, or blockers when `require_idle=true` is set and restart is blocked.
- Notes:
    - Restarts the supervised agent process. Start/restart uses the current `[agent]` config and the shared resolved environment, including selected provider credential bundles. Provider/model changes that require process reload are applied after restart.
    - `require_idle=true` returns blockers instead of restarting when active sessions have in-flight prompts or pending ACP permission requests.
    - `auto=true` queues a restart that runs once the same blockers clear.

### `GET /v1/agent/restart-blockers`

- Tier: `admin`
- Request: none.
- Response: `{ "target_id": "...", "blockers": [...] }`.
    - Blocker rows include `session_id`, `target_id`, `state`, and either prompt fields (`prompt_id`, `prompt_status`, `prompt_stop_reason`) or `permission_id`.
    - `state` values are `prompt_sent`, `working`, `permission_required`, and defensive `blocked`.
    - `prompt_status`, when present, is `pending` or `running`.
- Notes: returns active-session blockers for guarded restart.

### `POST /v1/agent/switch`

- Tier: `admin`
- Request: `{ "agent_id": "<id>", "provider": "<optional-provider-id>", "api_key_ref": "<optional-ref>", "drop": false }`.
- Response:
    - `provider_status` is one of `not_applicable`, `reused`, `set`, `selected`, `resumed`, or `no_op`.
    - The embedded `install` carries `outcome` of `installed` or `already_present`.
    - `models` entries are `{ "value" }` objects, the same entry shape as `GET /v1/models`.
    - Skill migration is reported as `skills_port` when the source and target skills directories differ: `status` of `shared`, `copied`, or `none_found`, with `copied`/`overwritten` entries, plus `kept_unmanaged` entries for same-named target skills that carry no managed marker and are therefore left untouched.
    - When the target declares a separate skills discovery directory, `skills_link` reports `linked`, `unchanged`, `conflicts`, `pruned`, and per-skill `errors` entries from the symlink refresh. A failed refresh does not fail the switch and is reported as `skills_link_error` instead.
    - Source cleanup failures are reported as `cleanup_errors` without rolling back a successful switch.
- Errors:
    - `409 agent.switch_conflict` — a same-target retry before the config write whose recomputed candidate does not match the journaled fingerprint.
    - `500 agent.switch_journal_corrupt` — an unreadable journal.
    - `400 request.invalid_param` — a same-target request carrying `provider`/`api_key_ref` or `drop` with explicit-intent violations.
- Notes:
    - The route validates provider compatibility, copies compatible provider secret refs when the target expects a different default ref, installs the target harness, provisions agent-owned config without a model, discovers ACP-advertised model values when the target supports model selection, writes canonical config, restarts the supervised agent only if it was already running, and optionally removes source agent-owned config.
    - `drop` does not delete secrets, installed harnesses/adapters, or sessions.

#### Switch Journal And Retry Semantics

The switch is journaled at `agent-switch.json` beside the canonical config so retries converge instead of failing as "already configured".

- The journal records the old/new target ids, the target agent id, a SHA-256 fingerprint of the canonical candidate config, whether the old agent was running, and a phase.
- The phase advances `planned` (written before the session rename and config write) → `committed` (after the config write and runtime refresh) → `runtime_applied` (after the stop/start re-apply) → `completed` (after optional source cleanup). The completed journal is retained and overwritten by the next switch.
- A same-target retry of an incomplete switch whose config write already landed resumes at the runtime re-apply with the journaled `was_running`. The pre-commit steps (install, provisioning, model discovery) keep their original results. The response reports `provider_status: "resumed"` with the pre-commit fields (`install`, `provisioned`, `models`, `secret_migrations`) omitted or empty.
- A same-target retry before the config write re-runs the full pipeline, but only if the recomputed candidate matches the journaled fingerprint; a mismatch is `409 agent.switch_conflict`.
- A retry of a completed switch is a side-effect-free no-op success reporting `provider_status: "no_op"` with `restarted: false` — no rewrite, no stop/start.
- Any different-target switch while a journal is incomplete, and any unreadable journal (`500 agent.switch_journal_corrupt`), fails rather than abandoning or compounding the in-flight switch.
- A bare same-target request (no `provider`, `api_key_ref`, or `drop`) with no incomplete journal to resume — journal absent, or completed for a target this request does not name — is accepted as already converged. It returns the same side-effect-free `provider_status: "no_op"` success as a completed-journal retry, without re-running install, provisioning, or the runtime re-apply.
- A same-target request that does carry `provider`/`api_key_ref` or `drop` keeps its explicit-intent `400 request.invalid_param` rejection.
- On a post-commit resume, `--drop` source cleanup cannot be reconstructed (the source target was renamed away) and is reported in `cleanup_errors` instead of running.

### `POST /v1/agent/config/native/inspect`

- Tier: `admin`
- Request: `{ "filename": "...", "content": "..." }`, capped at 1 MiB. Callers cannot supply a destination.
- Response: the SHA-256 revision, managed candidate ids and paths, blocked paths with reason codes, unmanaged paths, executable categories, and warnings. It never returns uploaded values, commands, headers, or secrets.
- Notes:
    - Parses an uploaded global config and returns a redacted review manifest.
    - The configured harness determines both parser and destination:
        - Claude Code `settings.json` → `~/.claude/settings.json`
        - Codex CLI `config.toml` → `~/.codex/config.toml`
        - OpenCode `opencode.json` or `opencode.jsonc` → normalized JSON at `~/.config/opencode/opencode.json`
        - Amp Code `settings.json` → `~/.config/amp/settings.json`
        - Pi `settings.json` → `~/.pi/agent/settings.json`
        - Goose `config.yaml` → `~/.config/goose/config.yaml`
    - Amp imports MCP servers only (it is provider-opaque and its model lives in ACP session config, not settings).
    - Pi imports its `defaultProvider`/`defaultModel` selection but no MCP (Pi has no first-class MCP in its settings file). Pi accepts only `settings.json`: `models.json`/`auth.json` carry literal credentials and `!shell-command` exec semantics, and `trust.json`/`mcp.json` are out of scope.
    - Goose imports its `GOOSE_PROVIDER`/`GOOSE_MODEL` selection and `extensions` MCP servers (stdio `cmd`/`args`/`env_keys` and remote `streamable_http` uris). `builtin`/`platform`/`frontend`/`inline_python` extensions and any literal `envs` block, `GOOSE_MODE`/`GOOSE_ALLOWLIST` are permissions, and the `GOOSE_PLANNER_*` keys are managed-unsupported. Goose accepts only `config.yaml`: `secrets.yaml` holds keyring-fallback API keys and `permission.yaml` carries per-tool approval levels.
    - The imported provider and model flow through canonical `acps` config, never persisted as `GOOSE_PROVIDER`/`GOOSE_MODEL` in the residual. Provisioning re-derives those from canonical config into the same `config.yaml`.

### `POST /v1/agent/config/native/import`

- Tier: `admin`
- Request: the inspected `revision`, repeatable candidate ids in `selected_managed_field_ids`, and `executable_settings_acknowledged`.
- Response: only `applied`, `queued`, `failed`, or `cancelled`, a sanitized canonical Agent projection, restart metadata, and a typed error code.
- Notes:
    - Applies the selected revision immediately or queues the complete transaction.
    - The executable-settings acknowledgement is required when the inspected revision contains unmanaged settings that can execute commands or load code.
    - A queued response means no live file has been changed; status and cancellation use its `operation_id`.
    - Terminal results stay queryable for 24 hours before pruning.

### `GET /v1/agent/config/native/import/{operation_id}`

- Tier: `admin`
- Request: none.
- Response: sanitized operation status and restart metadata.

### `POST /v1/agent/config/native/import/{operation_id}/cancel`

- Tier: `admin`
- Request: none.
- Response: standard envelope.
- Notes: cancels a queued import or rolls back the latest unchanged applied import. Cancel-of-applied rollback expires after 15 minutes.

### `POST /v1/agent/skills/add`

- Tier: `admin`
- Request: `{ "source": "<alias|github:owner[/repo]>", "skills": ["<selector>", ...] }`.
- Response: the install report plus a `skills_link`/`skills_link_error` refresh.
- Errors:
    - `400 request.invalid_param` (field `agent`) — the active agent is not a managed skills target.
    - `400 config.invalid` — missing or empty `skills` array.
- Notes:
    - Installs skills from a catalog alias, a configured alias, or `github:<owner>[/<repo>]` for the active agent. Downloads and installs each skill, skipping ones already installed.
    - The archive is fetched before the agent-config mutation lock is taken, which is held only for the copy into place.
    - Serializes with `POST /v1/agent/switch` through the agent-config mutation lock; `add` re-resolves the active agent under that lock before copying, so a switch that lands during the archive fetch redirects the install to the newly active agent.
    - Recorded as `skill.install` events in the runtime log.
    - Skill routes load config leniently (see source routes below).

### `POST /v1/agent/skills/remove`

- Tier: `admin`
- Request: `{ "skill": "<install-name>" }`.
- Response: `remove` plus the link refresh.
- Errors:
    - `404 agent.skill_not_installed` — the name is not installed.
    - `409 agent.skill_install_target_conflict` — the path exists but is not an acp-stack-managed skill (no `.acp-stack-managed` marker, or no regular `SKILL.md`). Manually added folders are never deleted.
    - `400 request.invalid_param` (field `agent`) — the active agent is not a managed skills target.
    - `400 request.invalid_param` (field `skill`) — a malformed `skill` name.
- Notes: deletes that skill and any emptied group directory. Recorded as `skill.remove` events in the runtime log. Serializes through the agent-config mutation lock like `add`.

### `POST /v1/agent/skills/sources/add`

- Tier: `admin`
- Request: `{ "alias", "github", "branch"?, "trusted"? }`.
- Response: the added source with the new source count.
- Errors: rejects an alias that shadows a catalog alias.
- Notes: registers a user skill source in `[[skills.sources]]`. Full config validation runs before the write. The write serializes through the agent-config mutation lock; it changes config only and installs nothing. Recorded as `skill.source_add` events in the runtime log.

### `POST /v1/agent/skills/sources/remove`

- Tier: `admin`
- Request: `{ "alias" }`.
- Response: standard envelope.
- Errors: `404 agent.skill_source_not_configured` — the alias is absent.
- Notes: removes a configured user skill source. Serializes through the agent-config mutation lock; changes config only and installs nothing. Recorded as `skill.source_remove` events in the runtime log.

#### Skill Route Config Handling

All skill routes load config leniently, dropping individually invalid `[[skills.sources]]` declarations the same way daemon startup does. One bad hand-edited entry therefore leaves the routes that repair it fully usable. A `sources/*` write canonicalizes that view back to disk, healing dropped entries out of the file with a warning per entry.

### `GET /v1/agent/status`

- Tier: `session`
- Request: none.
- Response: identity, process state, and sanitized configured/loaded providers.
    - Carries `configured_providers`, `loaded_providers`, and `provider_restart_required`.
    - Provider records contain only provider id, selected alias, and emitted env names.
    - `lifecycle_events` includes the `agent.update.*` events recorded by `POST /v1/agent/update`.
- Notes:
    - When configured-provider resolution fails (missing, unselected, or corrupt credential) the endpoint still returns with `configured_providers` empty and a remote-safe `provider_error` message, so monitoring stays reachable in the broken state. `/v1/array/status` isolates this per target rather than failing the whole fleet.
    - The loaded snapshot is recorded after a successful spawn and cleared on stop or exit.

### `GET /v1/agent/capabilities`

- Tier: `session`
- Request: none.
- Response: the latest ACP capability snapshot when available.
- Errors: `404 agent.not_initialized` — occurs only when neither the init capability probe nor agent start has run.
- Notes: populated by the init capability probe as well as by agent start.

### `GET /v1/agent/config-options`

- Tier: `session`
- Request: none.
- Response: `{ "agent_id", "config_options" }` — the full advertised option set from a fresh provisional `session/new`, in the same entry shape as `GET /v1/sessions/{id}/config-options`. Includes the categories the typed lanes do not carry (`model_config`, `_`-prefixed customs, category-less options) and boolean kinds.
- Errors: discovery failure is a hard error here. There is no catalog fallback, and an empty list would be indistinguishable from an agent that advertises nothing.
- Notes: each call spawns a provisional agent probe (same cost and timeout as `/v1/models`), so callers should not poll it. The typed `modes`/`efforts` arrays on `/v1/models` remain the pickers for the `agent.mode`/`agent.effort` settings and are not deprecated by this superset.

### `GET /v1/agent/update/status`

- Tier: `session`
- Request: none.
- Response: `{ "agent_id", "managed", "reason"?, "pinned"?, "auto_update": { "enabled", "frequency" }, "components": [{ "step", "status", ... }] }`.
    - `pinned` is the configured `harness_version` (harness/install step only).
    - `auto_update` reports the effective policy; an absent `[agent.auto_update]` section is reported as `enabled: false` with the default frequency.
    - Component `status` is `up_to_date` (`version`), `stale` (`installed`, `latest`), `unknown` (`reason`), or `not_installed`.
    - `managed` is false with empty `components` for a non-registry agent.
- Notes:
    - Returns installed, latest, pinned, and auto-update policy per managed component.
    - An upstream lookup failure degrades that component to `unknown` rather than failing the request.
    - Component comparison is always against the floating upstream latest, not the pin. A pinned agent sitting at its pin reports `stale` once upstream moves past it, while the update trigger still targets the pin and reports `up_to_date`. Callers rendering a pinned agent should compare `pinned` against the component's `installed` and treat `latest` as informational.
    - The `latest` lookups are live upstream calls (no caching). The npm client's timeout is 30 seconds per npm-backed component, so callers should set their request timeout above that.

### `GET /v1/agent/skills`

- Tier: `session`
- Request: none.
- Response: `{ "agent_id", "supported", "install_dir"?, "skills": [{ "name", "path", "source"? }] }`.
    - `source` is the source id recorded in the skill's managed marker at install time. It is absent for skills the user placed in the install root by hand (which `remove` refuses to delete).
    - `supported` is false and `skills` empty for an agent that is not a managed skills target.
- Notes: lists Agent Skills installed for the active agent.

### `GET /v1/agent/skills/catalog`

- Tier: `session`
- Request: none.
- Response: `{ "sources": [{ "id", "alias", "name", "repo", "catalog", "trusted", "skills", "essential" }] }` — the curated catalog plus configured user sources.
    - `catalog` is true for the embedded catalog and false for `[[skills.sources]]` entries.
    - `skills` are the selectors accepted by add (empty for user sources — use the source route to enumerate those live).

### `GET /v1/agent/skills/source`

- Tier: `session`
- Request: `?source=<ref>` — a catalog alias, a configured alias, or `github:<owner>[/<repo>]`.
- Response: `{ "id", "repo", "branch", "catalog", "trusted", "skills": [{ "selector", "name", "description"?, "path" }] }`.
- Errors: `400 agent.skill_install_invalid_source` — an unresolvable ref.
- Notes: resolves the ref, then downloads the source and lists its skills plus metadata.

### `GET /v1/providers`

- Tier: `session`
- Request: none.
- Response: provider ids available for the configured agent.

### `GET /v1/models`

- Tier: `session` (also mounted on the `bootstrap` tier of `acps init serve`, where the bootstrap bearer token replaces the session key, so a hosted backend renders pickers while init is still running).
- Request: optional `?target_id=<id>` (alias `?target=`) query param selecting a non-default Array target.
- Response: `{ "agent_id", "source", "models": [{ "value", "display_name"? }], "modes": [...], "efforts": [...], "catalog_error"? }`.
    - `efforts` carries the agent's ACP-advertised reasoning-effort values (the `thought_level` session config option) and is empty when the agent exposes no such option.
    - `source` is `"provider_catalog"` when models come from the provider's live model listing (`models_url` in the embedded provider metadata, fetched with the stored API key and cached at `~/.config/acp-stack/provider-models.json`) and `"acp_advertised"` when they come from the agent's ACP `session/new` config options.
    - `catalog_error` is present when the provider declares a model listing endpoint but the catalog is unavailable (fetch failed and nothing cached). The response then falls back to ACP-advertised values, which is an empty `models` list for agents without ACP model discovery (Hermes Agent).
- Notes:
    - Lists model and mode choices from the provider catalog or ACP discovery.
    - The catalog serves only mapped providers of agents whose harness takes the model verbatim from on-disk config (Claude Code profiled providers, Codex with OpenRouter, Hermes Agent). Custom providers have no listing endpoint, and agents with real ACP discovery keep their advertised list.
    - On the catalog path an ACP discovery failure degrades to `modes: []` and `efforts: []` instead of failing the request.
    - `?target_id=<id>` (alias `?target=`) discovers against that Array target instead of the default (primary) target. The id is validated the way other agent-target inputs are, so with Array mode off any non-primary id (and an unknown id generally) is a `400 request.invalid_param`, never a silent fallback to the default.
    - On the bootstrap tier the picker reads the on-disk config, which a fresh init writes early in the run (before agent install). A call made before init has staged the config returns `409 init.config_not_ready`; retry once setup has progressed past config staging.

## Array

### `GET /v1/array/status`

- Tier: `session`
- Request: none.
- Response: enabled flag, primary target, readiness, and per-target process/provider state.

### `GET /v1/array/targets/{target_id}/capabilities`

- Tier: `session`
- Request: none.
- Response: the latest ACP capability snapshot for one target.

### `POST /v1/array/targets/{target_id}/install`

- Tier: `admin`
- Request: none.
- Response: the `install` object (`outcome` of `installed` or `already_present`), same shape as `POST /v1/agent/install`.
- Notes: installs one target's harness.

### `POST /v1/array/targets/{target_id}/start`

- Tier: `admin`
- Request: none.
- Response: standard envelope.
- Notes: starts one target's process.

### `POST /v1/array/targets/{target_id}/stop`

- Tier: `admin`
- Request: none.
- Response: standard envelope.
- Notes: stops one target's process.

### `POST /v1/array/targets/{target_id}/restart`

- Tier: `admin`
- Request: none.
- Response: standard envelope.
- Notes: restarts one target's process.

#### Array Addressing Rules

- The `/v1/agent/*` routes operate on the Array `primary_target`.
- Session read and terminal routes address a specific target through the `target_id` query parameter (alias `target`).
- Session create, load, resume, and fork take `target_id` (alias `target`) in the JSON body. `POST /v1/sessions/{id}/prompt` takes it in the query.
- An unknown `target_id` returns `400 request.invalid_param`.
- With Array off, only the primary target is addressable for driving session ops, and start/restart of a non-primary target is rejected with `400`. Terminal ops (`close`, `cancel`) can still wind down a session on any stored target.
- See [array.md](../array.md) for the full Array model.

## Sessions

### `POST /v1/sessions`

- Tier: `session`
- Request: creates a new ACP session. Accepts optional `cwd` and `target_id` (alias `target`) in the JSON body.
- Response: the created session. May carry an `ignored` array (see capability-ignored rules below).
- Notes:
    - Session `cwd` values must be existing directories that canonicalize under `[workspace].root`. Stored CWD defaults are rechecked before reuse.
    - Closed sessions cannot be loaded, resumed, forked, or prompted.

#### Capability-Ignored Rules (create/load/resume)

Session creation proceeds when a configured `agent.mode`, model, `agent.effort`, or `[agent.config_options]` entry falls outside the agent's advertised `session/new` config options:

- The session proceeds on the agent's default.
- The response carries an `ignored` array (`[{ "feature": "agent.mode"|"agent.model"|"agent.effort"|"agent.config_option", "value", "capability", "reason", "option_id"? }]`, omitted when empty).
- `option_id` is present only on `agent.config_option` records, naming the dropped map entry.
- A warn-level `session.capability_ignored` session event records the omission.
- A failure from setting an advertised option is still an error.

### `GET /v1/sessions`

- Tier: `session`
- Request: filters accept `limit`, time bounds, and range values. Duration suffixes such as `30m`, `12h`, `60d`, `8w`, `6mo`, and `1y` are interpreted relative to request time.
- Response: `{ sessions, agent_sync }`.
    - `agent_sync` is `{ attempted, status, upserted, updated }`, reporting the optional ACP session-list sync.
    - `status` is `synced`, `unsupported`, or `not_running`. The latter two mean the durable list may be stale.
- Notes: lists durable sessions, optionally after ACP session-list sync.

### `GET /v1/sessions/-/status`

- Tier: `session`
- Request: `window=<duration>` from `1m` through `999h`. Defaults to a rolling `8h` activity window.
- Response: compact windowed session turn status. Each row includes a derived `state`: `idle`, `prompt_sent`, `working`, `permission_required`, `done`, `stopped`, `error`, `cancelled`, `available`, or `closed`.
    - `done` means the latest prompt completed with `stop_reason = "end_turn"`.
- Notes: also exposed on the local Unix socket without bearer auth.

### `GET /v1/sessions/{id}`

- Tier: `session`
- Request: none.
- Response: one session.

### `POST /v1/sessions/{id}/load`

- Tier: `session`
- Request: optional `cwd` and `target_id` (alias `target`) in the JSON body.
- Response: standard envelope with the capability-ignored rules above.
- Notes: loads an existing agent session. Explicit load/resume CWDs are stored after the agent accepts the call.

### `POST /v1/sessions/{id}/resume`

- Tier: `session`
- Request: optional `cwd` and `target_id` (alias `target`) in the JSON body.
- Response: standard envelope with the capability-ignored rules above.
- Notes: resumes a session. Explicit load/resume CWDs are stored after the agent accepts the call.

### `POST /v1/sessions/{id}/fork`

- Tier: `session`
- Request: optional `cwd`, `target_id` (alias `target`), and `{ "message_id": "<prompt message id>" }`.
- Response: standard envelope.
- Errors: `501 agent.unsupported_capability` — unsupported fork capabilities.
- Notes: forks a session through ACP. `message_id` requires an acknowledged ACP prompt message id from the parent session.

### `POST /v1/sessions/{id}/prompt`

- Tier: `session`
- Request: asynchronous. Body is `{ "prompt": "text" }` or `{ "prompt": [ <ACP content block>, … ] }` (camelCase ACP content blocks). Takes `target_id` (alias `target`) in the query.
    - A bare string becomes one text block.
    - `null`, an empty array, and any other JSON type are rejected.
- Response: a prompt id.
- Errors:
    - `400 prompt.unsupported_modality` — media-bearing prompt with confidently unsupported image, audio, or video input for the selected target model.
    - `409 session.prompt_in_flight` — the session already has a prompt the runtime is still driving. Retryable once that turn settles or is cancelled.
- Notes:
    - Clients can poll the prompt status endpoint or subscribe to `sessions.{id}` over WebSocket.
    - One session carries one turn at a time. A submission that arrives while the previous prompt is live is refused without creating a row, and never dispatched to the agent.
    - Before a prompt row is created, media-bearing prompts are checked against the selected target model's known input modalities from `models.dev`. Unknown models, unavailable catalog data, PDFs, and generic files are allowed through.

#### Prompt-Path Error Codes

Terminal prompt failures surface through the prompt row's `error_code` and through the matching session-scoped event:

| `error_code`           | HTTP status | Description                                                                            |
| ---------------------- | ----------- | -------------------------------------------------------------------------------------- |
| `agent.inference_5xx`  | 502         | Upstream inference endpoint returned 5xx (or the 529-overloaded variant)               |
| `agent.inference_4xx`  | 424         | Upstream inference endpoint returned 4xx (rate limit, malformed request)               |
| `agent.request_failed` | 502         | Agent rejected the ACP request for a non-inference reason                              |
| `prompt.stalled`       | n/a         | Sweeper-written code on rows it flipped to `stalled`; not surfaced as an HTTP response |

The `agent.inference_*` codes carry a sanitized public message of the form `"inference endpoint returned <status_code> (<reason_category>)"`, where `reason_category` is drawn from a fixed static enum. No URLs, request/response bodies, headers, or secret material reach the API response or the persisted prompt row. See [state-logging.md](../state-logging.md) for the full taxonomy and event shapes.

### `GET /v1/sessions/{id}/commands`

- Tier: `session`
- Request: none.
- Response: `{ available_commands, updated_at }` — the slash-command list the agent last advertised over ACP `available_commands_update`, and when the stored list last changed.
    - Entries are `{ name, description, input_hint? }`, names without a leading slash.
    - Both are empty/`null` when nothing has been advertised.

### `POST /v1/sessions/{id}/commands`

- Tier: `session`
- Request: `{ "command": "name", "args": "optional string" }`. A leading `/` on `command` is stripped.
- Response: the prompt-submit shape plus an advisory `advertised` boolean.
    - `false` means the command was absent from the stored list (the agent may ignore it).
    - The field is omitted when no list was ever advertised.
- Errors: `409 session.prompt_in_flight` — the session already has a live prompt, as on the prompt route.
- Notes: runs an agent slash command as a prompt. Submits the composed `/name args` text through the normal prompt pipeline. The submission is never blocked on the list, which can be stale and does not bound what the agent accepts.

### `GET /v1/sessions/{id}/config-options`

- Tier: `session`
- Request: none.
- Response: `{ config_options, updated_at }` — the session's ACP config options as last observed.
    - Seeded from `session/new` at create, refreshed by `session/set_config_option` responses and `config_option_update` notifications. Empty/`null` for sessions created before any snapshot existed.
    - Each entry is `{ id, name, description?, category?, type: "select"|"boolean", current_value, options? }`.
    - `category` is the ACP category verbatim (including `model_config`, `_`-prefixed customs, and future reserved values; absent when the agent advertised none).
    - `current_value` is a string for selects and a boolean for booleans. `options` lists select choices flattened across groups.
    - Option kinds this runtime cannot encode are omitted.

### `POST /v1/sessions/{id}/config-options`

- Tier: `session`
- Request: `{ "config_id", "value" }` (string for selects, boolean for booleans).
- Response: the refreshed config-option list, or the stored snapshot when the agent responded with an empty list.
- Errors: `400 request.invalid_param` — a `config_id` or value the stored snapshot does not carry. Retryable. The check is skipped while the snapshot is empty, letting the agent arbitrate.
- Notes:
    - Sets one session config option. Forwards `session/set_config_option` to the live agent and rewrites the stored snapshot from the refreshed list in the response.
    - An agent responding with an empty list leaves the snapshot untouched; the response then serves the stored snapshot, which the agent's own `config_option_update` notification refreshes.
    - A successful set records an info-level `session.config_option_set` session event naming the `config_id`.

### `POST /v1/sessions/{id}/cancel`

- Tier: `session`
- Request: none.
- Response: standard envelope.
- Errors: `502 agent.request_failed` — the live prompt did not settle as `cancelled`.
- Notes:
    - Cancels an in-flight prompt. ACP `session/cancel` goes out first, then the runtime waits up to 20 seconds for the live prompt row to reach a terminal status.
    - Success means the prompt row is already `cancelled` when the response returns.
    - A prompt that instead reaches `completed` or `errored` means the agent ended the turn on its own terms, and the call fails. The prompt row keeps whatever status the agent produced.
    - A prompt still running when the wait expires also fails the call, and the turn stays live: the row stays non-terminal, a new prompt for that session is refused with `409 session.prompt_in_flight`, and cancel can be retried.
    - Idempotent when the session has no live prompt: the notification goes out and the call succeeds.

### `DELETE /v1/sessions/{id}`

- Tier: `session`
- Request: none.
- Response: standard envelope.
- Notes:
    - Closes the agent-side session and preserves local history.
    - The runtime calls ACP `session/close` when supported, marks the local row `closed`, and keeps durable events/query history.
    - Permanent deletion is deferred until product semantics are defined.

### `POST /v1/sessions/{id}/delete`

- Tier: `session`
- Request: none.
- Response: includes a `deleted` boolean.
- Errors: `501 agent.unsupported_capability` — the agent does not advertise `sessionCapabilities.delete`. The local row is kept.
- Notes:
    - Forwards ACP `session/delete` and hard-deletes local history.
    - Idempotent: an unknown or already-deleted id returns success with `deleted: false` and never dials the agent.

### `GET /v1/sessions/{id}/prompts/{prompt_id}`

- Tier: `session`
- Request: none.
- Response: prompt status.
- Notes:
    - Prompt status values are `pending`, `running`, `completed`, `errored`, `cancelled`, and `stalled`.
    - `stalled` is a terminal status reached only when the stale-prompt sweeper observes no ACP `session/update` activity for longer than `[prompts].stale_threshold`.
    - From the client's perspective, a `stalled` prompt is final: it will not transition back to `running`, and recovery means submitting a new prompt.
    - See [runtime.md](../runtime.md) for the sweeper contract.

### `GET /v1/sessions/{id}/events`

- Tier: `session`
- Request: `after=<event_id>` paginates forward on `(created_at, id)` ascending.
- Response: durable session events.
- Notes: used for forward catch-up after a snapshot read (`after=last_event_id`).

### `GET /v1/sessions/{id}/changes`

- Tier: `session`
- Request: none.
- Response: the process-local ACP file-diff snapshot, identified by `generation` plus `revision`.
    - `truncated: true` means whole tool calls were omitted by a capacity limit.
- Notes:
    - Reduces explicit ACP `type: "diff"` tool-call content into the latest tool-call snapshot.
    - A missing `oldText` is returned as `null` and represents a created file.
    - Tool-call-update content replaces the prior collection when present and otherwise leaves it intact.
    - The snapshot is bounded and process-local. It is not rebuilt from SQLite after restart.
    - Raw `session.update` event persistence and WebSocket delivery are unchanged.

### `GET /v1/sessions/{id}/snapshot`

- Tier: `session`
- Request: none.
- Response: the reconnect-bootstrap helper. Carries:
    - `session` — full session row (id, status, agent id, cwd, title, metadata). The durable `status` is `active`, `available`, or `closed`, distinct from the derived `state` of the status route.
    - `in_flight_prompts` — prompts currently in `pending` or `running`, capped at 25 (`SNAPSHOT_IN_FLIGHT_PROMPTS_CAP`). Empty when the session is idle. Each entry is the same shape returned by `GET /v1/sessions/{id}/prompts/{prompt_id}`.
    - `last_event_id` — the id of the newest persisted session event, or `null` when the session has no events. Acts as a tail cursor for forward catch-up via `GET /v1/sessions/{id}/events?after=last_event_id`.
    - `recent_events` — the latest session events, newest-first, capped at 50. The cap is enforced by `SNAPSHOT_RECENT_EVENTS_LIMIT` in `src/api/routes/sessions.rs` and is sized to cover one prompt-turn's worth of updates without bloating the response.
    - `available_commands` — the agent's last advertised slash-command list, same entry shape as `GET /v1/sessions/{id}/commands`. Empty when nothing has been advertised; may be stale until the agent re-advertises.
- Notes:
    - Reconnect flow: `GET snapshot` once to recover state, subscribe to `sessions.{id}` over WebSocket, then `GET events?after=last_event_id` to catch up on events that landed between the snapshot read and the WebSocket subscribe.
    - For deeper history (older than the 50-event snapshot window), additional pagination is not currently exposed. Older events are reachable only through the durable logs endpoints.

## Workspace Files

Workspace routes are session-tier. Paths are workspace-relative. The runtime rejects absolute paths, NUL bytes, `..` traversal, symlink escapes, writes through existing symlink targets, and files above `workspace.max_file_bytes`.

### `GET /v1/workspace`

- Tier: `session`
- Request: none.
- Response: workspace metadata.

### `GET /v1/files?path=...`

- Tier: `session`
- Request: `path` query parameter.
- Response: directory entries. Each entry carries a `kind` of `file`, `directory`, `symlink`, or `other`.

### `GET /v1/files/content?path=...`

- Tier: `session`
- Request: `path` query parameter.
- Response: `{ ..., "encoding" }` with `encoding` of `utf8` or `base64`. Reads a file as UTF-8 or base64.

### `PUT /v1/files/content`

- Tier: `session`
- Request: `{ "path", "encoding", "content" }`, where `encoding` is `utf8` or `base64`.
- Response: standard envelope.
- Notes: writes a file atomically.

### `POST /v1/files/upload`

- Tier: `session`
- Request: multipart with required `path` and `file` fields.
- Response: standard envelope.
- Notes: uploads one file below `workspace.uploads`.

### `GET /v1/files/download?path=...`

- Tier: `session`
- Request: `path` query parameter.
- Response: streams raw file bytes. Not wrapped in the response envelope.

### `DELETE /v1/files?path=...`

- Tier: `session`
- Request: `path` query parameter.
- Response: standard envelope.
- Notes: deletes one regular file.

#### Size Cap

`workspace.max_file_bytes` caps reads, writes, uploads, and downloads. Oversized files return `413 workspace.too_large`.

## Commands

Commands are session-tier and mediated by policy.

### `POST /v1/commands`

- Tier: `session`
- Request: starts or queues a shell command. Body shape:

    ```json
    {
        "command": "rg TODO .",
        "cwd": ".",
        "env": { "NAME": "value" },
        "timeout": "10m"
    }
    ```

- Response: the command record.
- Notes:
    - Command status values are `pending`, `running`, `exited`, `failed`, and `cancelled`.
    - Command records include `last_output_event_id`, `last_output_at`, `last_output_seq`, `output_bytes`, and `last_progress_at` for reconnect and liveness checks, plus `origin` (`operator` for gateway submissions, `acp` for agent-created client terminals) and `session_id` (set on `acp`-origin rows).

### `GET /v1/commands`

- Tier: `session`
- Request: none.
- Response: command records.

### `GET /v1/commands/{id}`

- Tier: `session`
- Request: none.
- Response: one command record.

### `GET /v1/commands/{id}/output`

- Tier: `session`
- Request: `limit`, `after`, and `order=asc|desc`.
- Response: `{ chunks, next_cursor }`. Each chunk is shaped as `{ event_id, created_at, command_id, stream, seq, data }`.
- Notes: returns persisted output chunks. Output is persisted up to the configured byte cap and streamed on the `commands.{id}` WebSocket topic while the command runs.

### `POST /v1/commands/{id}/cancel`

- Tier: `session`
- Request: none.
- Response: standard envelope.
- Notes: cancels a running command.

#### Command Reconnect Flow

Read `GET /v1/commands/{id}`, subscribe to `commands.{id}`, then query `/output?order=asc&after=<last-seen-event-id>` to catch chunks missed between the HTTP read and WebSocket subscribe.

## Permissions

Permission requests are created by ACP permission callbacks and by mediated commands when policy requires review.

- Composed mediated commands using shell control operators, command substitution, or process substitution require review before execution, including in `permissions.mode = "auto"`.
- Policy matching considers shell-word-normalized command words, so constructed spellings such as quoted or escaped command names can be denied or routed to review.
- Cancellation is not an HTTP operation: pending requests are cancelled internally when their owning flow ends (session close, mediated-command cancel).

### `GET /v1/permissions/pending`

- Tier: `session`
- Request: none.
- Response: pending requests.

### `GET /v1/permissions/{id}`

- Tier: `session`
- Request: none.
- Response: a single permission request.

### `POST /v1/permissions/{id}/approve`

- Tier: `session`
- Request: `{ "option_id": "<id>"?, "reason": "<text>"? }`.
- Response: standard envelope.
- Errors: `409 permission.invalid_transition` — the request is already terminal, including approving a permission whose command has since died. Clients should treat it as "the request was already settled" (the decision event names the cause), not as a retryable error.
- Notes: for an ACP-source request, `option_id` names one of the ids in the request's `detail.options` and is forwarded to the agent as-is (not validated against that list). Omitting it selects the request's first option. A command-source request has no options.

### `POST /v1/permissions/{id}/deny`

- Tier: `session`
- Request: `{ "reason": "<text>"? }`.
- Response: standard envelope.
- Errors: `409 permission.invalid_transition` — the request is already terminal.

## Dependencies

The runtime derives every package-manager command from config-declared install actions. Only install actions declared in config can be applied. System-scope actions escalate through `sudo -n` when the daemon is non-root and passwordless sudo is available; otherwise the `installer_runs` row is recorded as `privilege_required`.

### `GET /v1/deps`

- Tier: `session`
- Request: none.
- Response: declared dependency status.

### `POST /v1/deps/check`

- Tier: `session`
- Request: none.
- Response: dependency status after re-check.

### `POST /v1/deps/apply`

- Tier: `admin`
- Request: `{ "confirmation": true, "feature": "<name>"? }`.
    - `confirmation` defaults to `false`, which yields a side-effect-free preview (`applied: false`, `candidates` only, no `report`).
    - `feature` filters candidates.
- Response: a confirmed apply installs and returns `report.apply_run_id` for correlating dependency audit rows.
    - Each result's `outcome.kind` is one of `installed`, `already_present`, `privilege_required`, or `failed`.
- Errors: `409 deps.apply_in_flight` — another apply is live. The error reports its `apply_run_id`; poll that run before retrying.
- Notes: runs declared install actions. Every confirmed apply records a durable run and shares one cross-process apply slot with `acps deps apply` and init applies. Repeating the request after a terminal partial failure is safe because already-installed dependencies report `already_present`.

### `GET /v1/deps/apply/runs`

- Tier: `session`
- Request: optional `limit` query parameter, default `50` and capped at `1000`.
- Response: recorded apply runs newest first. Each includes status, origin, timestamps, progress, outcome counts, liveness, retryability, log directory, and an optional error.

### `GET /v1/deps/apply/runs/latest`

- Tier: `session`
- Request: none.
- Response: the newest apply run plus its per-action `installer_runs` rows. Action metadata never includes captured log contents.
- Errors: `404 deps.apply_run_not_found` — no run has been recorded.

### `GET /v1/deps/apply/runs/{apply_run_id}`

- Tier: `session`
- Request: the apply run id in the path.
- Response: the selected apply run plus its per-action `installer_runs` rows.
- Errors: `404 deps.apply_run_not_found` — the id is unknown.

A `running` row is reconciled to `failed` with `error.code = "deps.apply_abandoned"` once it is abandoned: its owning process is gone, or the daemon left it running after a terminal-state write failed and clears it before the next apply. `retryable` is true for `failed` and `privilege_blocked` runs.

## Status, Logs, Metrics, And Security

### `GET /v1/status`

- Tier: `session`
- Request: none.
- Response: local status summary.
    - Carries `deps_apply_in_flight`, true while any API, CLI, synchronous-init, or detached-init dependency apply is live. It is derived from the daemon's apply lock and the PID-checked durable run. Callers preparing to restart or reconfigure should wait for it to clear; detached install children survive daemon restarts.
    - Carries a `server` object (see below).

#### Server Object And Feature Flags

`GET /v1/status` and `GET /v1/health/live` both carry a `server` object with the running version and the capabilities this build advertises:

```json
{
  "version": "0.1.9",
  "features": ["network-provider-workload-env", "agent-test-json", "managed-credential-base-url"]
}
```

- `features` exists because `version` is not a usable capability signal: a nightly build carries its fourth version component only in the git tag, so a nightly with a feature and one without report the same three-part base version.
- Orchestrators that gate wire calls or config writes on a capability must test membership in `features`. An absent or empty list means none of the listed capabilities are present.
- The names are a stable contract:
    - `network-provider-workload-env` — `[extensions.<name>.workload_env]`
    - `agent-test-json` — `acps agent test --format json`
    - `managed-credential-base-url` — `base_url` on a managed-state credential selection

### `GET /v1/status/agent`

- Tier: `session`
- Request: none.
- Response: alias for agent status.

### `GET /v1/status/connections`

- Tier: `session`
- Request: none.
- Response: active HTTP request count.

### `GET /v1/health/live`

- Tier: `session`
- Request: none.
- Response: process liveness. Carries the `server` object described above.

### `GET /v1/health/ready`

- Tier: `session`
- Request: none.
- Response: subsystem readiness summary; `503` when degraded.
    - Returns an envelope-shaped body it builds itself (not a typed `ApiSuccess`, so it is not schema-covered), with `ok` mirroring readiness. A `503` carries `ok: false` alongside `data` and no `error` object.
    - Includes an `mcp` object for configured MCP declarations:

```json
{
  "configured_count": 1,
  "failing_count": 0,
  "servers": [
    {
      "name": "linear",
      "kind": "http",
      "ok": true
    }
  ]
}
```

    - Stdio server rows may include `command_path`. Failing rows may include `missing_secret_refs` and `reason`.
    - HTTP MCP readiness validates declaration shape and secret refs only; it does not call the remote MCP endpoint.
    - Readiness also reports orphaned agent process groups under `agent.orphaned_process_count` and `agent.orphaned_process_pids`. Any live process group from an older `agent.started` lifecycle row, excluding the currently supervised PID, degrades readiness with `agent` in `failing`.

### `GET /v1/security/check`

- Tier: `admin`
- Request: none.
- Response: findings.
- Notes: runs the self-check and persists the run.

### `GET /v1/security/history`

- Tier: `admin`
- Request: none.
- Response: persisted self-check runs, newest-first.

### `GET /v1/security/history/{run_id}`

- Tier: `admin`
- Request: none.
- Response: a single self-check run with findings.

### `GET /v1/logs/events`

- Tier: `session`
- Request: the shared log filters plus `level`, `kind`, `source`, `session_id`, `command_id`, `permission_id`, and `category`.
- Response: durable event rows.

### `GET /v1/logs/commands`

- Tier: `session`
- Request: the shared log filters plus `status`.
- Response: command history.

### `GET /v1/logs/permissions`

- Tier: `session`
- Request: the shared log filters plus `kind`, `source`, and `permission_id`.
- Response: permission history.

### `GET /v1/logs/security`

- Tier: `session`
- Request: the shared log filters plus `category` and the per-stream cursors `auth_failures_after` and `events_after`.
- Response: security events in two streams (`auth_failures` and `events`).
- Notes:
    - `order` applies to both result streams.
    - A shared `after` is only the fallback for each per-stream cursor. The two streams page independently, so reusing one cursor for both mis-pages.
    - `category` accepts the security-category labels documented in [state-logging.md](../state-logging.md) and constrains only the `events` stream.

### `GET /v1/logs/sessions`

- Tier: `session`
- Request: the shared log filters plus `status`.
- Response: session-scoped history.

#### Shared Log Filters

Log query filters are per-route, not one shared set. All log routes accept:

- `limit` — default 100; values above 1000 are clamped, not rejected.
- `since`, `until`, `after`.
- `order` — `asc` or `desc`, default `desc`.
- `kind` matches by exact value or, with a trailing `.`, by dotted-namespace prefix.

### `GET /v1/metrics/summary`

- Tier: `session`
- Request: `since` and `until`, each an RFC 3339 timestamp or a duration suffix (`1h`, `30m`, `2d`). They default to 24h-ago and now.
- Response: aggregate metrics for the time window.
    - A `window` object echoing the resolved `[since, until)` bounds.
    - A `counts` object of ten totals.
    - `prompt_failures`, so operators can separate upstream inference outages from local runtime failures. Contains `total`, explicit counters for each `failure_class` (`inference_5xx`, `inference_4xx`, `agent_request`, `vm`, `sqlite`, `daemon`, `agent_process`, `stalled`), `by_class`, and inference event breakdowns by HTTP status code and reason category.
    - The `api_connections` block: `request_count`, `average_duration_ms`, `by_status` response buckets, and count maps by method, route template, key kind, event source, origin kind, country code, and region code. Any missing or empty grouping key (including an unauthenticated request's key kind) is bucketed under `unknown`. `request_count` is always present (`0` on an empty window).
- Errors: `400` — `since > until`.
- Notes: the counts exclude `/v1/ws`, `/v1/health/*`, and `/v1/status*` for public-tier callers; internal `local`-tier calls are still counted.

### `GET /v1/installer/runs`

- Tier: `session`
- Request: query parameters.
    - `active=true` returns only in-flight (`running`) steps, oldest first, ignoring `limit`.
    - `agent=<id>` scopes to one agent id (`deps_apply` covers dependency installs).
    - `limit` caps history rows (default 100, max 1000).
- Response: rows from the `installer_runs` table (agent installs and updates, plus `deps_apply` rows).
    - Each row carries `{ "id", "agent_id", "operation", "step", "method", "status", "started_at", "finished_at", "exit_status", "version" }`.
    - Running rows additionally carry `elapsed_seconds`, computed server-side so pollers need no clock sync with the daemon.
    - `operation` is `install` or `update`. `step` is `install`, `harness`, `adapter`, or `deps_apply`. `method` is `shell`, `npm`, `github`, `apt`, or `native`.
    - `status` is `running`, `ran`, `failed`, `error`, `timeout`, `skipped`, `config_error`, `installed`, or `privilege_required`.
- Notes:
    - This is the live-progress surface for harness/adapter installs, which can run for minutes. A platform driving instance init polls `?active=true&agent=<id>` to render step-level progress while `POST /v1/agent/install` (or a switch) is in flight.
    - Step stdout/stderr previews and the on-disk log directory are never returned; logs stay on the host.
    - A row that remains `running` means the daemon died mid-step. Treat it as stale, not as progress.

## WebSocket

### `GET /v1/ws`

- Tier: `session`
- Request: WebSocket upgrade. Clients authenticate with the session key and send a `{ "type": "subscribe", "topics": [...] }` frame to subscribe. Frames of any other `type` are ignored.
- Response: the WebSocket event stream.
- Notes: topics are `logs`, `workspace`, `permissions`, `status`, `commands.{id}`, `sessions.{id}`, and `agent.lifecycle`.

### `GET /v1/ws/connections`

- Tier: `session`
- Request: none.
- Response: active connections without raw secrets.

### `GET /v1/ws/sessions`

- Tier: `session`
- Request: none.
- Response: session-topic subscriptions.

### `POST /v1/ws/connections/disconnect`

- Tier: `admin`
- Request: `{ "connection_ids": [...], "reason": "<text>"? }`.
- Response: standard envelope.
- Notes: disconnects the listed connections. The optional `reason` is recorded on the resulting `ws.client_disconnected` event as `operator_reason` (present only when supplied); the event's `reason` stays the machine cause `operator_disconnect`.

### `POST /v1/ws/sessions/disconnect`

- Tier: `admin`
- Request: `{ "session_ids": [...], "reason": "<text>"? }`.
- Response: standard envelope.
- Notes: disconnects subscribers to the listed sessions. Same `reason`/`operator_reason` semantics as the connections disconnect route.
