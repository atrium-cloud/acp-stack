# API Spec

All public HTTP routes are versioned under `/v1`. Clients authenticate with a bearer API key:

```http
Authorization: Bearer <key>
```

## Auth Tiers

| Tier        | Used for                                                                        |
| ----------- | ------------------------------------------------------------------------------- |
| Session key | sessions, workspace files, mediated commands, logs, status, pending permissions |
| Admin key   | secrets, config import, agent process control, security-sensitive operations    |
| Local       | internal Unix socket used by keyless local `acps` routes                        |

`acps init` creates the session and admin keys on first run, prints the plaintext once, and stores only local verifier rows. The session key can be rotated by an admin-authenticated daemon call. The admin key is regenerated only by resetting and reinitializing the instance.

Public HTTP tiering is strict. `[local].session_auth = "keyless"` only affects same-user Unix-socket access and never makes admin keys valid for public session routes.

## Response Envelope

JSON success responses:

```json
{ "ok": true, "data": {} }
```

JSON errors:

```json
{
  "ok": false,
  "error": {
    "code": "config.invalid",
    "message": "workspace.root must be absolute",
    "details": {}
  }
}
```

Binary downloads and WebSocket frames are not wrapped in this envelope. `GET /v1/health/ready` returns an envelope-shaped body it builds itself (not a typed `ApiSuccess`, so it is not schema-covered), with `ok` mirroring readiness — a `503` carries `ok: false` alongside `data` and no `error` object.

## Machine-Readable Schema

A JSON Schema (draft 2020-12) of the typed contract is generated from the Rust wire types and committed at `docs/specs/api/acps-schema.json`, with a `docs/specs/api/acps-schema.meta.json` sidecar carrying the schema version and per-namespace definition counts. Consumers reference it two ways: the raw committed file at a git ref — `https://raw.githubusercontent.com/atrium-cloud/acp-stack/<ref>/docs/specs/api/acps-schema.json` (pin `main` for the current contract, or a tag for a reproducible one) — or the release asset `https://github.com/atrium-cloud/acp-stack/releases/latest/download/acps-schema.json`, which resolves to the newest stable release (nightlies are prereleases, so `latest` skips them).

The document splits `$defs` into three namespaces because serde's `#[serde(default)]`/`skip_serializing_if` fields belong to `required` under one direction but not the other: `request` (what a client sends, deserialize contract), `response` (what the server emits, serialize contract), and `config` (the `acps-config.toml` shape). A type used on both sides appears once per namespace.

Cross-field rules (mutually-required/exclusive fields, exactly-one-of headers, blank-as-absent) are mostly not expressible in JSON Schema and stay enforced in code, documented in these specs and in the field descriptions of the highest-traffic request bodies rather than duplicated as schema logic; the few that map cleanly to a structural keyword are emitted (e.g. the config file's at-least-one-of `agent`/`array` sections as an `anyOf`), with the finer per-field checks still in the loader. Not covered by the schema, by design: WebSocket frames (`/v1/ws` `LiveEvent` and the init streaming frames — hand-built and byte-pinned), the envelope-bypassing binary download handler and the hand-built `health/ready` handler, and the untyped `config` import response. Coverage of the typed handler surface is verified in CI (`cargo run --features dev-tools --bin generate-api-schema -- --coverage`); regenerate after any DTO change with `cargo run --features dev-tools --bin generate-api-schema`.

## Bootstrap Init API

`acps init serve` exposes only the bootstrap init routes below. Normal session/admin `/v1` routes are not mounted in this mode. Calls use exactly one `Authorization: Bearer <bootstrap-token>` header; the token comes from process input, not config or state.

| Route                                      | Contract |
| ------------------------------------------ | -------- |
| `POST /v1/init/sessions`                  | starts one active init session and accepts optional initial agent/provider/model/workspace args, environment configuration declarations, plus an in-memory native config upload |
| `GET /v1/init/sessions/{id}`              | returns non-secret status, the `signals` replay, pending input, recent progress, `last_activity_age_secs`, and `completed_awaiting_ack` when a result exists |
| `GET /v1/init/sessions/{id}/events?after_seq=N` | replays non-secret progress, signal, and input lifecycle events; bounded to recent history — the authoritative full replay is the `signals` field of the status body and `hello` |
| `GET /v1/init/sessions/{id}/ws`           | upgrades to the hosted init WebSocket |
| `POST /v1/init/sessions/{id}/cancel`      | cancels the active session (reason `backend_cancel`) and returns the post-cancel `{session_id, status}`; a no-op on a session already `closed`/`cancelled`/`errored`/`completed_awaiting_ack` |
| `POST /v1/init/sessions/{id}/native-config/cancel` | cancels a queued native-config import or rolls back the latest applied one; body `{operation_id, revision}`; `409 init.result_unavailable` unless a result is awaiting acknowledgement (see [init.md](../init.md)) |

`POST /v1/init/sessions` returns `{ "session_id": "...", "status": "running" }` in the standard success envelope. It returns `409 init.session_active` while another session is running or awaiting result acknowledgement. Unknown request fields are rejected. The session `status` is one of `running`, `waiting_for_input`, `completed_awaiting_ack`, `errored`, `cancelled`, or `closed`; the same set backs the status route and the cancel route's `{session_id, status}` body.

The request body groups into:

- Agent/provider/model/mode/effort: `agent`, `provider`, `api_key_ref`, `model`, `mode`, `effort`, `custom_provider`, `provider_name`, `base_url`, `provider_api`, `model_name`, `context`, `output_max_tokens`, `skip_testflight`, `testflight`, `native_config` (`{ "filename", "content" }`). `mode` is the initial session mode id, validated against the agent's ACP-advertised `mode` values by the same provisional session that discovers models; declaring it here skips the streamed mode picker. `effort` is the initial reasoning-effort value, validated the same way against the agent's advertised `thought_level` values, and declaring it skips the streamed effort picker. `api_key_ref` and `custom_provider` each require `provider`, and `provider_name`, `base_url`, `provider_api`, `model_name`, `context`, and `output_max_tokens` each require `custom_provider: true`; a field arriving without its anchor is a `400` naming the offending field, never echoing its value.
- Custom agent (the escape-hatch agent, mirroring the `--custom-agent-*` flags): `custom_agent_id`, `custom_agent_name`, `custom_agent_command`, `custom_agent_args` (array), `custom_agent_install`, `custom_agent_creates`. Custom-agent prompts are never streamed, so the whole spec must arrive here. Every other field in this group requires `custom_agent_id`; `custom_agent_id` in turn requires non-blank `custom_agent_command` and `custom_agent_install`, and conflicts with `agent`, `provider`, `model`, `mode`, `effort`, and `custom_provider: true`. Violations are a `400` naming the offending field, never echoing its value. Reserved and registry-colliding ids are rejected later, in-session, where the registry is in hand.
- Run selection: `resume` and `fresh` booleans, matching `--resume` and `--fresh`; they conflict with each other. `resume` continues the most recent unfinished or failed run instead of starting another — recorded arguments replay, and any field set in this request layers on top like an explicit flag. Hosted init rotates keys on every run, including a resumed one.
- Workspace: `workspace_root`, `workspace_uploads`, `runtime_user`, `sandbox`, `code_from` (array of git URLs), `data_from` (array of local paths or https archive URLs).
- Environment configuration (mirrors the `acps init` flag and wizard surface; these replace the interactive environment wizard, which is never streamed to hosted clients — the one exception is the post-install MCP step, whose prompts stream when this request declared no MCP server):
  - `mcp_preset`: array of preset ids (`linear`).
  - `mcp_stdio`: array of `{ "name", "command", "args": [...], "env": [...] }`; each `env` entry is a bare secret ref name exported into the server's environment, or a `VAR=template` entry whose template interpolates `${SECRET_REF}` (rules in [config.md](../config.md)).
  - `mcp_http`: array of `{ "name", "url", "headers": [{ "name", "value_ref"? , "value"? }] }`; each header sets exactly one of `value_ref` (whole-value secret ref) or `value` (a `${SECRET_REF}`-interpolated template). URLs must be https, or http to a loopback host (a local relay endpoint).
  - `skills_source` + `skills`: explicit skill selection, same semantics as `--skills-source`/`--skills`; both must be declared together. `essential_skills`: boolean, conflicts with the explicit pair. An unsatisfiable skills declaration (e.g. the selected agent has no Agent Skills install directory) fails the init session.
  - `deps` and `deps_system`: arrays of `{ "name", "shell" }` install records (user/system scope). `deps_apply` + `deps_apply_yes`: booleans, must be set together (the interactive apply confirmation is never streamed); when set, init runs the declared install actions, otherwise dependencies are declared in config but not installed. `standard_agent_work_deps` and `browser_use`: booleans enabling the standard dependency bundle and the browser-use profile.
  - `data_sources`: array of tagged records — `{ "type": "local", "path" }`, `{ "type": "https", "url", "expected_sha256"?, "max_download_bytes"?, "max_extracted_bytes"? }`, or `{ "type": "s3", "bucket", "region", "prefix"?, "access_key_ref", "secret_key_ref" }` — each with an optional `name`.
- Update policies (mirror the `--stack-update`/`--agent-update` flags; declared up-front, never streamed): `stack_update` (`on` | `security` | `off`) with optional `stack_update_frequency` (day/week units, e.g. `1d`, `3w`); `agent_update` (`on` | `off`) with optional `agent_update_frequency` (hour/day/week units, e.g. `12h`, `1d`). Each `*_frequency` requires its policy — a frequency with no policy is a `400`. Omitted policies leave the config schema defaults intact; `agent_update` is honored only for managed registry agents, and `agent_update: "on"` against a custom agent fails the session.
- `defer_provider_credentials` (boolean, default `false`): the caller's declaration that it will push a configured custom provider's credential through the managed-state extension after init. Only when set does a missing custom-provider api-key ref soft-pass — init reports the deferral as progress and continues; otherwise, and for mapped providers regardless of this flag, a missing provider ref fails the session exactly as a terminal run would. The declaration replaces the prior behavior of inferring the deferral from the mere presence of a hosted driver.

The create route validates request shape, the cross-field rules above, and the MCP secret-value positions in full — env entries and header `value_ref`/`value` are screened for pasted-credential shapes (rejected without echoing the value), then checked for ref-name and template syntax, and headers violating the exactly-one rule are rejected — all as a `400` at the boundary. Remaining semantic validation of field values (MCP URL scheme rules, data-source paths, and the enumerated values of `stack_update`, `agent_update`, `sandbox`, and `provider_api`) happens in-session, so a declaration invalid only in those ways returns `200` with `"status": "running"` and then fails the init session.

Secret values referenced by these declarations (MCP `env`/`value_ref` entries, refs inside `${}` templates, S3 key refs) are never carried in the request body: init collects any refs missing from the secret store over the prompt stream as `password` inputs with `required: false`. Answering with `null` skips a ref without failing the session; a skipped MCP secret later surfaces through runtime MCP health, and a skipped S3 key ref fails workspace materialization. Provider key ref prompts stream `required: true` instead: a `null` answer is accepted but a still-unresolved provider ref fails the session, unless the request declared `defer_provider_credentials` and the provider is a configured custom provider, in which case the ref defers to the post-init managed-state push. The values never appear in status or event replay.

Status and event replay never include plaintext session/admin keys or secret input values. Pending input includes `request_id`, `kind`, `style`, `prompt`, `required`, optional `default`, and per-option `index`, `value`, `label`, and `hint`. `kind` is the machine-readable prompt identity (`agent`, `provider_id`, `model`, `mode`, `effort`, `mcp_transport`, `secret_ref_value`, and so on) and is the field a client routes on; `style` remains the rendering hint. Option `value` is a stable id that survives display rewording, so answers may address a choice as `{"value": "<id>"}` in addition to the existing index, label, and `null` forms; an unknown value is rejected as an invalid parameter. A native upload produces `style: "native_config_review"` plus the redacted `inspection`; its client response value is the revision-bound selection object used by the normal import contract. Client `input` frames must include the active `request_id`; stale input is rejected. Unknown fields on a client frame are ignored rather than rejected. One such field is defined: an `input` frame answering the `testflight_confirm` prompt may carry `deferred: true` beside `value: false`, which tells init the answer is a hosting backend that will run the testflight itself after setup rather than an operator declining it — the testflight step then reports `testflight: deferred (runs after setup)` and records the `SkipDeferred` decision instead of `SkipDeclined`. The flag is ignored on every other prompt and on an accepting answer.

WebSocket server frames are `hello`, `progress`, `signal`, `input_required`, `input_accepted`, `result`, and `error`. Client frames are `input`, `cancel`, `replay_result`, `ack_result`, `replay_error`, and `ack_error`. The final `result` frame carries the platform handoff payload and always includes plaintext `session_key` and `admin_key`: hosted init generates them on a fresh instance and rotates them over pre-existing state, so a keyless result cannot occur.

Each `signal` frame reports one raw fact the wizard observed — a step starting or finishing, or a category becoming applicable, settled, or failed — so a client renders progress from structure instead of parsing `progress` text. Signals are seq-bearing events: they appear in event replay (`after_seq`) alongside `progress`, one event per fact and no dedup. The instance forwards these facts and does not compute a rendered category view; the client folds the stream into one. `hello` and the status response body carry a `signals` array — the whole stream so far, in order — so a client that connects late, or reconnects after the bounded event history evicted early events, folds the same input as a full-stream client. The signal set is bounded by init's structure (a fixed set of steps and nine categories), so the replay is safe to carry in full.

The replay entries are the same seq-bearing frames a live client receives, so a client re-folds from the `hello`/status replay and drops any live frame whose `seq` is at or below the replay's `last_seq`. `hello` and status are the authoritative replay; `events?after_seq` is bounded to recent history and may not reach the oldest signals. A client must ignore signal types, applicability `source` values, and prompt kinds it does not recognize rather than failing, so the stream can gain variants without breaking existing clients. On an `init.ws_lagged` frame the server re-sends `hello`; the client re-folds it and continues.

```json
{ "signal": "step_started", "step": "provider_configure", "seq": 12, "session_id": "...", "type": "signal" }
{ "signal": "step_finished", "step": "provider_configure", "disposition": "executed", "error_code": "init.provider_write_failed", "seq": 13, "session_id": "...", "type": "signal" }
{ "signal": "category_applicability", "category": "mcp", "applicable": false, "source": "probe", "reason": "agent does not advertise MCP support", "seq": 14, "session_id": "...", "type": "signal" }
{ "signal": "category_settled", "category": "agent", "value": "opencode", "seq": 15, "session_id": "...", "type": "signal" }
{ "signal": "category_provisionally_settled", "category": "mode", "value": "smart", "seq": 16, "session_id": "...", "type": "signal" }
{ "signal": "category_failed", "category": "skills", "code": "init.skills_install_failed", "seq": 17, "session_id": "...", "type": "signal" }
```

`error_code` is present only on a failed step; `reason` only on an inapplicable verdict; a `category_settled` `value` is `null` for a settlement that wrote nothing. `source` is one of `args`, `registry`, `probe`, `discovery`, `discovery_unavailable`. Settlement values are ids and secret ref names only (the provider settles with the configured provider id, not the key behind it), never secret values.

The fold produces all ten categories in this order: `agent`, `provider`, `model`, `mode`, `effort`, `workspace`, `native_config`, `mcp`, `skills`, `deps`. Each takes a `status`, resolved in the precedence below — a category qualifying for more than one takes the first that matches:

- `failed` — with the typed error `code` that broke the lane. A lane that broke did run, so failure outranks a `not_applicable` verdict that arrived before it.
- `not_applicable` — this run has no such lane (`registry` says the agent takes no provider, a `probe` found no MCP support, `discovery` found no modes, the operator skipped workspace init). A `reason` names what ruled the lane out; it is the only status that carries one. Authority is ranked: a `probe`, `discovery`, or `discovery_unavailable` verdict is the installed harness talking, so a later `registry` claim never revives the lane.
- `awaiting_input` — the pending prompt belongs to this category. The client derives this from `pending_input` by mapping the prompt's `kind` to a category: `agent` → `agent`; `provider_id`, `provider_name`, `base_url`, `api_key_ref`, `provider_api_key_value` → `provider`; `model` → `model`; `mode` → `mode`; `effort` → `effort`; `native_config_review` → `native_config`; the MCP prompts (`mcp_add`, `mcp_transport`, `mcp_row_action`, the `mcp_stdio_*` and `mcp_http_*` kinds) → `mcp`; every other kind — secret-ref, testflight, config-source, custom-agent, skills, dependency, data-source, and update-policy prompts — maps to no category and leaves nothing awaiting. A prompt awaits from its `input_required` until the matching `input_accepted`; at most one category holds `awaiting_input`, since there is one pending input at a time.
- `settled` — done, with an optional `value` naming what was written. A settlement (`category_settled`) and a failure are this run's own evidence and are never withdrawn as inapplicable, though a settled lane still moves to `failed` if the step behind it breaks afterwards. A `category_provisionally_settled` value — a value read off configuration that predates the run, what a resumed or fully declared run reports for its provider, model, and mode lanes — is withdrawn, value and all, when a `probe` or `discovery` verdict finds the installed agent no longer has the lane, but never merely because that live check could not be made (`discovery_unavailable`).
- `blocked` — waiting on the category named in `blocked_on`. `provider` waits on `agent`, `model` on `provider`, `mode` and `effort` on `model`, and both `mcp` and `skills` on `agent`; `workspace`, `native_config`, and `deps` wait on nothing.
- `ready` — applicable, unblocked, not yet settled.

The client also folds `current_step` from the `step_started`/`step_finished` stream: the last step named, using the step-kind vocabulary (`agent_install`, `capability_probe`, `mcp_configure`, `provider_configure`, and so on), `null` before the first step. Steps with no category of their own move no category. A `step_finished` with no `error_code` settles the step's category if nothing else did, and the `init_complete` step settling successfully sweeps every still-open applicable lane to `settled`; a failed final step runs no sweep. Cross-cutting prompts that belong to no category (`secret_ref_value`, `testflight_confirm`, and the other setup kinds) leave nothing awaiting input. A failing step emits `step_finished` with an `error_code` before the terminal `error` frame; the fold badges the step's category `failed`, unless another lane already claimed that same code (`provider_configure` covers the provider, model, mode, and effort lanes, so whichever broke badges itself) or the lane is one this run does not have — those two guards keep a step error from inventing or duplicating a badge, and there the `error` frame carries the failure alone. A specific lane may also fail on its own through a `category_failed` signal emitted by the code that owns it (a provider write, say), which the fold applies unconditionally. A failure that owns no running step emits no category-bearing signal at all. The frontier freezes with the session: once it is cancelled, closed, errored, or awaiting result acknowledgement, no later `signal` is recorded and the replay in `hello` and the status route stops growing, so the terminal frame is the last word on what the run settled.

Environment declarations the installed agent's capabilities do not cover do not fail the session: they are written to config, skipped at runtime, and reported only through the result payload's `ignored_features` (see [init.md](../init.md#platform-handoff-json)), never through `progress` frames.

After `result`, the session remains `completed_awaiting_ack`. If the WebSocket drops before acknowledgement, the backend reconnects and sends `replay_result`; the server does not replay keys through status or generic events. `ack_result` is terminal: the server clears the in-memory handoff payload, closes the session, and exits successfully.

A failure after key handover still delivers a `result` frame (with `"status": "failed"` and any freshly generated keys) through the normal result/ack path above. A failure with no result payload to deliver — before key handover completed — parks symmetrically instead: the session enters `errored` and the server stays up so the backend can learn the typed failure instead of a dead port. The `error` payload is available through the status route, the reconnect `hello` frame, and `replay_error`; `ack_error` releases the server, which exits non-zero. `cancel` is a no-op on a parked failure, like on an un-acked result. `POST /v1/init/sessions` returns `409 init.session_active` while a failure is parked. If no `ack_error` arrives within a 2-minute grace (enforced regardless of `--idle-timeout` and of connected WebSockets), the server expires the error with reason `error_ack_timeout` and exits non-zero on its own.

The server also self-terminates abandoned sessions: after `--idle-timeout` (default `15m`) with no connected WebSocket and no API activity, or once `--max-lifetime` elapses, the session is cancelled (reason `idle_timeout`/`max_lifetime`), any un-acknowledged result is discarded, attached WebSockets are closed server-side after the final event, and the process exits non-zero — including when a limit is reached before any session was created (the pre-session idle clock runs from the last authenticated API call). A WebSocket disconnect restarts the idle clock, leaving the full timeout for the documented reconnect-and-`replay_result` flow. `last_activity_age_secs` in the status response is the idle time leading up to that request, measured before the request itself counts as activity, and supports backend-side reap decisions.

## Config And Secrets

| Route                                | Tier    | Contract                                                     |
| ------------------------------------ | ------- | ------------------------------------------------------------ |
| `GET /v1/config/export`              | session | returns current canonical TOML with secret refs only         |
| `POST /v1/config/validate`           | session | validates raw TOML without writing                           |
| `POST /v1/config/import`             | admin   | validates and writes canonical TOML; supports `dry_run=true` |
| `GET /v1/secrets`                    | admin   | lists secret names only                                      |
| `POST /v1/secrets`                   | admin   | stores or replaces a secret value                            |
| `DELETE /v1/secrets/{name}`          | admin   | deletes a secret                                             |
| `POST /v1/auth/session-key/regenerate` | admin | replaces the session verifier and returns the new plaintext key once |
| `PUT /v1/auth/local-session-access`  | admin   | sets `[local].session_auth` and applies it to the running daemon |
| `POST /v1/admin/extensions/{name}/apply` | admin | applies one managed-state registry revision to the named extension namespace |

Secret values are never returned by the API. Auth keys are not secret-store entries.

`POST /v1/admin/extensions/{name}/apply` is the managed-state extension seam: `{name}` must resolve to a declared `type = "managed-state"` instance (else `404 extensions.not_found`), the body is `{schema_version, revision, desired}` with responses in the standard envelope, revision-ordering conflicts are `409 extensions.revision_conflict`, provenance refusals are `400 extensions.state_ownership`, and a provider id that is neither mapped nor configured as a custom provider is `400 request.invalid_param`. Full contract in [extensions.md](../extensions.md).

## Agent And Providers

| Route                        | Tier    | Contract                                                      |
| ---------------------------- | ------- | ------------------------------------------------------------- |
| `POST /v1/agent/install`     | admin   | installs the configured supported agent                       |
| `POST /v1/agent/update`      | admin   | runs the managed agent update on demand and returns the per-step report |
| `POST /v1/agent/start`       | admin   | starts the supervised agent process                           |
| `POST /v1/agent/stop`        | admin   | stops the supervised agent process                            |
| `POST /v1/agent/restart`     | admin   | restarts the supervised agent process                         |
| `GET /v1/agent/restart-blockers` | admin | returns active-session blockers for guarded restart        |
| `POST /v1/agent/switch`      | admin   | switches harness, installs it, and returns model choices      |
| `POST /v1/agent/config/native/inspect` | admin | parses an uploaded global config and returns a redacted review manifest |
| `POST /v1/agent/config/native/import` | admin | applies the selected revision immediately or queues the complete transaction |
| `GET /v1/agent/config/native/import/{operation_id}` | admin | returns sanitized operation status and restart metadata |
| `POST /v1/agent/config/native/import/{operation_id}/cancel` | admin | cancels a queued import or rolls back the latest unchanged applied import |
| `POST /v1/agent/skills/add`  | admin   | installs skills from a catalog alias, a configured alias, or `github:<owner>[/<repo>]` for the active agent |
| `POST /v1/agent/skills/remove` | admin | uninstalls one installed skill from the active agent          |
| `POST /v1/agent/skills/sources/add` | admin | registers a user skill source in `[[skills.sources]]`      |
| `POST /v1/agent/skills/sources/remove` | admin | removes a configured user skill source                 |
| `GET /v1/agent/status`       | session | returns identity, process state, and sanitized configured/loaded providers |
| `GET /v1/agent/capabilities` | session | returns the latest ACP capability snapshot when available     |
| `GET /v1/agent/config-options` | session | returns every session config option the agent advertises on a provisional session |
| `GET /v1/agent/update/status` | session | returns installed, latest, pinned, and auto-update policy per managed component |
| `GET /v1/agent/skills`       | session | lists Agent Skills installed for the active agent              |
| `GET /v1/agent/skills/catalog` | session | lists catalog and configured user skill sources            |
| `GET /v1/agent/skills/source` | session | inspects one source (`?source=`) and lists its skills + metadata |
| `GET /v1/providers`          | session | lists provider ids available for the configured agent         |
| `GET /v1/models`             | session | lists model and mode choices from the provider catalog or ACP discovery |

`GET /v1/agent/capabilities` is populated by the init capability probe as well as by agent start; `404 agent.not_initialized` occurs only when neither has run.

`GET /v1/models` returns `{ "agent_id", "source", "models": [{ "value", "display_name"? }], "modes": [...], "efforts": [...], "catalog_error"? }`. `efforts` carries the agent's ACP-advertised reasoning-effort values (the `thought_level` session config option) and is empty when the agent exposes no such option. `source` is `"provider_catalog"` when models come from the provider's live model listing (`models_url` in the embedded provider metadata, fetched with the stored API key and cached at `~/.config/acp-stack/provider-models.json`) and `"acp_advertised"` when they come from the agent's ACP `session/new` config options. The catalog serves only mapped providers of agents whose harness takes the model verbatim from on-disk config (Claude Code profiled providers, Codex with OpenRouter, Hermes Agent); custom providers have no listing endpoint, and agents with real ACP discovery keep their advertised list. `catalog_error` is present when the provider declares a model listing endpoint but the catalog is unavailable (fetch failed and nothing cached); the response then falls back to ACP-advertised values, which is an empty `models` list for agents without ACP model discovery (Hermes Agent). On the catalog path an ACP discovery failure degrades to `modes: []` and `efforts: []` instead of failing the request.

`GET /v1/agent/config-options` returns `{ "agent_id", "config_options" }` — the full advertised option set from a fresh provisional `session/new`, in the same entry shape as `GET /v1/sessions/{id}/config-options`, including the categories the typed lanes do not carry (`model_config`, `_`-prefixed customs, category-less options) and boolean kinds. Discovery failure is a hard error here: there is no catalog fallback, and an empty list would be indistinguishable from an agent that advertises nothing. Each call spawns a provisional agent probe (same cost and timeout as `/v1/models`), so callers should not poll it. The typed `modes`/`efforts` arrays on `/v1/models` remain the pickers for the `agent.mode`/`agent.effort` settings and are not deprecated by this superset.

Agent start/restart uses the current `[agent]` config and the shared resolved environment, including selected provider credential bundles. Status returns `configured_providers`, `loaded_providers`, and `provider_restart_required`; provider records contain only provider id, selected alias, and emitted env names. When configured-provider resolution fails (missing, unselected, or corrupt credential) the status endpoint still returns with `configured_providers` empty and a remote-safe `provider_error` message, so monitoring stays reachable in the broken state; `/v1/array/status` isolates this per target rather than failing the whole fleet. The loaded snapshot is recorded after a successful spawn and cleared on stop or exit. Provider/model changes that require process reload are applied after restart. `POST /v1/agent/restart?require_idle=true` returns blockers instead of restarting when active sessions have in-flight prompts or pending ACP permission requests. `POST /v1/agent/restart?auto=true` queues a restart that runs once the same blockers clear. `GET /v1/agent/restart-blockers` returns `{ "target_id": "...", "blockers": [...] }`; blocker rows include `session_id`, `target_id`, `state`, and either prompt fields (`prompt_id`, `prompt_status`, `prompt_stop_reason`) or `permission_id`. State values are `prompt_sent`, `working`, `permission_required`, and defensive `blocked`; `prompt_status`, when present, is `pending` or `running`.

Native config inspection accepts `{ "filename": "...", "content": "..." }`, capped at 1 MiB. The configured harness determines both parser and destination: Claude Code `settings.json` becomes `~/.claude/settings.json`, Codex CLI `config.toml` becomes `~/.codex/config.toml`, OpenCode `opencode.json` or `opencode.jsonc` becomes normalized JSON at `~/.config/opencode/opencode.json`, Amp Code `settings.json` becomes `~/.config/amp/settings.json`, Pi `settings.json` becomes `~/.pi/agent/settings.json`, and Goose `config.yaml` becomes `~/.config/goose/config.yaml`. Amp imports MCP servers only (it is provider/model-opaque); Pi imports its `defaultProvider`/`defaultModel` selection but no MCP (Pi has no first-class MCP in its settings file). Pi accepts only `settings.json`: `models.json`/`auth.json` carry literal credentials and `!shell-command` exec semantics, and `trust.json`/`mcp.json` are out of scope. Goose imports its `GOOSE_PROVIDER`/`GOOSE_MODEL` selection and `extensions` MCP servers (stdio `cmd`/`args`/`env_keys` and remote `streamable_http` uris); `builtin`/`platform`/`frontend`/`inline_python` extensions and any literal `envs` block, `GOOSE_MODE`/`GOOSE_ALLOWLIST` are permissions, and the `GOOSE_PLANNER_*` keys are managed-unsupported. Goose accepts only `config.yaml`: `secrets.yaml` holds keyring-fallback API keys and `permission.yaml` carries per-tool approval levels. The imported provider and model flow through canonical `acps` config, never persisted as `GOOSE_PROVIDER`/`GOOSE_MODEL` in the residual; provisioning re-derives those from canonical config into the same `config.yaml`. Callers cannot supply a destination. The inspection response contains the SHA-256 revision, managed candidate ids and paths, blocked paths with reason codes, unmanaged paths, executable categories, and warnings. It never returns uploaded values, commands, headers, or secrets.

Import accepts the inspected `revision`, repeatable candidate ids in `selected_managed_field_ids`, and `executable_settings_acknowledged`. The executable-settings acknowledgement is required when the inspected revision contains unmanaged settings that can execute commands or load code. Responses expose only `applied`, `queued`, `failed`, or `cancelled`, a sanitized canonical Agent projection, restart metadata, and a typed error code. A queued response means no live file has been changed; status and cancellation use its `operation_id`. Terminal results stay queryable for 24 hours before pruning; cancel-of-applied rollback expires after 15 minutes.

`POST /v1/agent/switch` accepts `{ "agent_id": "<id>", "provider": "<optional-provider-id>", "api_key_ref": "<optional-ref>", "drop": false }`. The route validates provider compatibility, copies compatible provider secret refs when the target expects a different default ref, installs the target harness, provisions agent-owned config without a model, discovers ACP-advertised model values when the target supports model selection, writes canonical config, restarts the supervised agent only if it was already running, and optionally removes source agent-owned config. Source cleanup failures are reported as `cleanup_errors` without rolling back a successful switch. `drop` does not delete secrets, installed harnesses/adapters, or sessions. The response's `provider_status` is one of `not_applicable`, `reused`, `set`, `selected`, `resumed`, or `no_op`, and the embedded `install` (also the body of `POST /v1/agent/install` and `POST /v1/array/targets/{id}/install`) carries `outcome` of `installed` or `already_present`; `models` entries are `{ "value" }` objects, the same entry shape as `GET /v1/models`. Skill migration is reported as `skills_port` (`status` of `shared`, `copied`, or `none_found`, with `copied`/`overwritten` entries, plus `kept_unmanaged` entries for same-named target skills that carry no managed marker and are therefore left untouched) when the source and target skills directories differ. When the target declares a separate skills discovery directory, `skills_link` reports `linked`, `unchanged`, `conflicts`, `pruned`, and per-skill `errors` entries from the symlink refresh; a failed refresh does not fail the switch and is reported as `skills_link_error` instead.

The switch is journaled at `agent-switch.json` beside the canonical config so retries converge instead of failing as "already configured". The journal records the old/new target ids, the target agent id, a SHA-256 fingerprint of the canonical candidate config, whether the old agent was running, and a phase that advances `planned` (written before the session rename and config write) → `committed` (after the config write and runtime refresh) → `runtime_applied` (after the stop/start re-apply) → `completed` (after optional source cleanup); the completed journal is retained and overwritten by the next switch. A same-target retry of an incomplete switch whose config write already landed resumes at the runtime re-apply with the journaled `was_running` — the pre-commit steps (install, provisioning, model discovery) are not re-run, and the response reports `provider_status: "resumed"` with the pre-commit fields (`install`, `provisioned`, `models`, `secret_migrations`) omitted or empty. A same-target retry before the config write re-runs the full pipeline, but only if the recomputed candidate matches the journaled fingerprint; a mismatch is `409 agent.switch_conflict`. A retry of a completed switch is a side-effect-free no-op success reporting `provider_status: "no_op"` with `restarted: false` — no rewrite, no stop/start. Any different-target switch while a journal is incomplete, and any unreadable journal (`500 agent.switch_journal_corrupt`), fails rather than abandoning or compounding the in-flight switch. A bare same-target request (no `provider`, `api_key_ref`, or `drop`) with no incomplete journal to resume — journal absent, or Completed for a target this request does not name — is accepted as already converged and returns the same side-effect-free `provider_status: "no_op"` success as a completed-journal retry, without re-running install, provisioning, or the runtime re-apply; a same-target request that does carry `provider`/`api_key_ref` or `drop` keeps its explicit-intent `400 request.invalid_param` rejection. On a post-commit resume, `--drop` source cleanup cannot be reconstructed (the source target was renamed away) and is reported in `cleanup_errors` instead of running.

The day-2 skill routes act on the active agent. `GET /v1/agent/skills` returns `{ "agent_id", "supported", "install_dir"?, "skills": [{ "name", "path", "source"? }] }`; `source` is the source id recorded in the skill's managed marker at install time and is absent for skills the user placed in the install root by hand (which `remove` refuses to delete). `supported` is false and `skills` empty for an agent that is not a managed skills target. `GET /v1/agent/skills/catalog` returns the curated catalog plus configured user sources as `{ "sources": [{ "id", "alias", "name", "repo", "catalog", "trusted", "skills", "essential" }] }`, where `catalog` is true for the embedded catalog and false for `[[skills.sources]]` entries, and `skills` are the selectors accepted by add (empty for user sources — use the source route to enumerate those live). `POST /v1/agent/skills/add` accepts `{ "source": "<alias|github:owner[/repo]>", "skills": ["<selector>", ...] }`, downloads and installs each skill (skipping ones already installed), and returns the install report plus a `skills_link`/`skills_link_error` refresh; the archive is fetched before the agent-config mutation lock is taken, which is held only for the copy into place. `POST /v1/agent/skills/remove` accepts `{ "skill": "<install-name>" }`, deletes that skill and any emptied group directory, and returns `remove` plus the link refresh; a name that is not installed is `404 agent.skill_not_installed`, and a path that exists but is not an acp-stack-managed skill (no `.acp-stack-managed` marker, or no regular `SKILL.md`) is `409 agent.skill_install_target_conflict` — manually added folders are never deleted. Successful `add`/`remove` are recorded as `skill.install`/`skill.remove` events in the runtime log. When the active agent is not a managed skills target, `add` and `remove` fail with `400 request.invalid_param` (field `agent`); `add` with a missing or empty `skills` array fails with `400 config.invalid`, and `remove` with a malformed `skill` name fails with `400 request.invalid_param` (field `skill`). Both mutations serialize with `POST /v1/agent/switch` through the agent-config mutation lock; `add` re-resolves the active agent under that lock before copying, so a switch that lands during the archive fetch redirects the install to the newly active agent.

`POST /v1/agent/update` runs the same managed update path as the auto-update timer, synchronously, for the configured agent (harness, plus adapter when the registry pairs one) — it works with `[agent.auto_update]` disabled or absent, which is the intended mode for platforms that own update scheduling themselves. The optional body is `{ "force": bool }` (default false); `force` reinstalls even when the resolved target version matches the installed one. The response is the updater report `{ "agent_id", "updated", "skipped", "reason"?, "steps": [{ "step", "status", "method"?, "installed"?, "latest"?, "message"? }] }` with step `status` of `updated`, `up_to_date`, `skipped`, or `failed`; `installed` is the version before the update and `latest` the resolved target (github/npm only — apt and native updates have no capturable version). `up_to_date` is a first-class no-op success. A running (or starting/stopping/updating) agent is never touched: the route returns `200` with `skipped: true` and reason `agent is running`, including for a second update request arriving while one is in flight, so callers may retry safely. A non-registry (escape-hatch) agent likewise returns `200` with `skipped: true`. A `harness_version` pin constrains the update target the same way it constrains install: the pinned GitHub Release tag is used instead of the latest release (harness component, github path only), and a pinned agent already at its pin reports `up_to_date`. Failed steps still return `200` with per-step `failed` status and `message`; only infrastructure errors (unreadable registry, state open failure) produce an error envelope. Each run records `agent.update.started` plus a terminal `agent.update.finished`/`agent.update.skipped`/`agent.update.failed` lifecycle event, payload-tagged with `"trigger": "api"` to distinguish it from the timer's runs; these surface in `GET /v1/agent/status` `lifecycle_events`.

`GET /v1/agent/update/status` returns per-component version visibility: `{ "agent_id", "managed", "reason"?, "pinned"?, "auto_update": { "enabled", "frequency" }, "components": [{ "step", "status", ... }] }`. `pinned` is the configured `harness_version` (harness/install step only), and `auto_update` reports the effective policy — an absent `[agent.auto_update]` section is reported as `enabled: false` with the default frequency. Component `status` is `up_to_date` (`version`), `stale` (`installed`, `latest`), `unknown` (`reason`), or `not_installed`; an upstream lookup failure degrades that component to `unknown` rather than failing the request. Component comparison is always against the floating upstream latest, not the pin: a pinned agent sitting at its pin reports `stale` once upstream moves past it, while the update trigger still targets the pin and reports `up_to_date` — callers rendering a pinned agent should compare `pinned` against the component's `installed` and treat `latest` as informational. `managed` is false with empty `components` for a non-registry agent. The `latest` lookups are live upstream calls (no caching); the npm client's timeout is 30 seconds per npm-backed component, so callers should set their request timeout above that.

The source routes manage `[[skills.sources]]`. `GET /v1/agent/skills/source?source=<ref>` resolves a catalog alias, a configured alias, or `github:<owner>[/<repo>]`, then downloads the source and returns `{ "id", "repo", "branch", "catalog", "trusted", "skills": [{ "selector", "name", "description"?, "path" }] }`; an unresolvable ref is `400 agent.skill_install_invalid_source`. `POST /v1/agent/skills/sources/add` accepts `{ "alias", "github", "branch"?, "trusted"? }`, rejects an alias that shadows a catalog alias, writes the entry to config (full config validation runs before the write), and returns the added source with the new source count. `POST /v1/agent/skills/sources/remove` accepts `{ "alias" }` and returns `404 agent.skill_source_not_configured` when the alias is absent. The add/remove writes serialize through the agent-config mutation lock; they change config only and install nothing, and record `skill.source_add`/`skill.source_remove` events in the runtime log. All skill routes load config leniently, dropping individually invalid `[[skills.sources]]` declarations the same way daemon startup does, so one bad hand-edited entry does not disable the routes that repair it; a `sources/*` write canonicalizes that view back to disk, healing dropped entries out of the file with a warning per entry.

## Array

| Route                                            | Tier    | Contract                                                            |
| ------------------------------------------------ | ------- | ------------------------------------------------------------------- |
| `GET /v1/array/status`                           | session | enabled flag, primary target, readiness, and per-target process/provider state |
| `GET /v1/array/targets/{target_id}/capabilities` | session | latest ACP capability snapshot for one target                       |
| `POST /v1/array/targets/{target_id}/install`     | admin   | installs one target's harness                                       |
| `POST /v1/array/targets/{target_id}/start`       | admin   | starts one target's process                                         |
| `POST /v1/array/targets/{target_id}/stop`        | admin   | stops one target's process                                          |
| `POST /v1/array/targets/{target_id}/restart`     | admin   | restarts one target's process                                       |

The `/v1/agent/*` routes operate on the Array `primary_target`. Session read and terminal routes address a specific target through the `target_id` query parameter (alias `target`); session create, load, resume, and fork take `target_id` (alias `target`) in the JSON body, and `POST /v1/sessions/{id}/prompt` takes it in the query. An unknown `target_id` returns `400 request.invalid_param`. With Array off, only the primary target is addressable for driving session ops and start/restart of a non-primary target is rejected with `400`, but terminal ops (`close`, `cancel`) can still wind down a session on any stored target. See [../array.md](../array.md) for the full Array model.

## Sessions

| Route                                       | Tier    | Contract                                                       |
| ------------------------------------------- | ------- | -------------------------------------------------------------- |
| `POST /v1/sessions`                         | session | creates a new ACP session                                      |
| `GET /v1/sessions`                          | session | lists durable sessions, optionally after ACP session-list sync |
| `GET /v1/sessions/-/status`                 | session | returns compact windowed session turn status                    |
| `GET /v1/sessions/{id}`                     | session | returns one session                                            |
| `POST /v1/sessions/{id}/load`               | session | loads an existing agent session                                |
| `POST /v1/sessions/{id}/resume`             | session | resumes a session                                              |
| `POST /v1/sessions/{id}/fork`               | session | forks a session through ACP                                    |
| `POST /v1/sessions/{id}/prompt`             | session | enqueues a prompt and returns a prompt id                      |
| `GET /v1/sessions/{id}/commands`            | session | returns the agent's last advertised slash-command list         |
| `POST /v1/sessions/{id}/commands`           | session | runs an agent slash command as a prompt                        |
| `GET /v1/sessions/{id}/config-options`      | session | returns the session's stored config-option snapshot            |
| `POST /v1/sessions/{id}/config-options`     | session | sets one session config option and returns the refreshed list  |
| `POST /v1/sessions/{id}/cancel`             | session | cancels an in-flight prompt                                    |
| `DELETE /v1/sessions/{id}`                  | session | closes the agent-side session and preserves local history      |
| `POST /v1/sessions/{id}/delete`             | session | forwards ACP `session/delete` and hard-deletes local history   |
| `GET /v1/sessions/{id}/prompts/{prompt_id}` | session | returns prompt status                                          |
| `GET /v1/sessions/{id}/events`              | session | returns durable session events                                 |
| `GET /v1/sessions/{id}/changes`             | session | returns the process-local ACP file-diff snapshot               |
| `GET /v1/sessions/{id}/snapshot`            | session | returns session row, in-flight prompts, and recent events      |

`POST /v1/sessions/{id}/prompt` is asynchronous; its body is `{ "prompt": "text" }` or `{ "prompt": [ <ACP content block>, … ] }` (camelCase ACP content blocks). A bare string becomes one text block; `null`, an empty array, and any other JSON type are rejected. Clients can poll the prompt status endpoint or subscribe to `sessions.{id}` over WebSocket.

`GET /v1/sessions/{id}/config-options` returns `{ config_options, updated_at }` — the session's ACP config options as last observed (seeded from `session/new` at create, refreshed by `session/set_config_option` responses and `config_option_update` notifications), empty/`null` for sessions created before any snapshot existed. Each entry is `{ id, name, description?, category?, type: "select"|"boolean", current_value, options? }`: `category` is the ACP category verbatim (including `model_config`, `_`-prefixed customs, and future reserved values; absent when the agent advertised none), `current_value` is a string for selects and a boolean for booleans, and `options` lists select choices flattened across groups. Option kinds this runtime cannot encode are omitted. `POST /v1/sessions/{id}/config-options` takes `{ "config_id", "value" }` (string for selects, boolean for booleans), forwards `session/set_config_option` to the live agent, rewrites the stored snapshot from the refreshed list in the response, and returns it; a `config_id` or value the stored snapshot does not carry is a retryable `400 request.invalid_param` (the check is skipped while the snapshot is empty, letting the agent arbitrate), and an agent responding with an empty list leaves the snapshot untouched — the response then serves the stored snapshot, which the agent's own `config_option_update` notification refreshes. A successful set records an info-level `session.config_option_set` session event naming the `config_id`.

`GET /v1/sessions/{id}/commands` returns `{ available_commands, updated_at }` — the slash-command list the agent last advertised over ACP `available_commands_update` (entries are `{ name, description, input_hint? }`, names without a leading slash) and when the stored list last changed; both are empty/`null` when nothing has been advertised. `POST /v1/sessions/{id}/commands` takes `{ "command": "name", "args": "optional string" }` (a leading `/` on `command` is stripped), submits the composed `/name args` text through the normal prompt pipeline, and returns the prompt-submit shape plus an advisory `advertised` boolean: `false` means the command was absent from the stored list (the agent may ignore it), and the field is omitted when no list was ever advertised. The submission is never blocked on the list, which can be stale and does not bound what the agent accepts.

`GET /v1/sessions` returns `{ sessions, agent_sync }`. `agent_sync` is `{ attempted, status, upserted, updated }` reporting the optional ACP session-list sync; `status` is `synced`, `unsupported`, or `not_running` (the latter two mean the durable list may be stale).

`POST /v1/sessions/{id}/delete` is idempotent: an unknown or already-deleted id returns success with `deleted: false` and never dials the agent. When the agent does not advertise `sessionCapabilities.delete`, the route returns HTTP 501 `agent.unsupported_capability` and the local row is kept.

Before a prompt row is created, media-bearing prompts are checked against the selected target model's known input modalities from `models.dev`. Confidently unsupported image, audio, or video input returns HTTP 400 `prompt.unsupported_modality`; unknown models, unavailable catalog data, PDFs, and generic files are allowed through.

Session create, load, resume, and fork accept an optional `cwd`. Session `cwd` values must be existing directories that canonicalize under `[workspace].root`; stored CWD defaults are rechecked before reuse. Explicit load/resume CWDs are stored after the agent accepts the call. Closed sessions cannot be loaded, resumed, forked, or prompted.

A configured `agent.mode`, model, `agent.effort`, or `[agent.config_options]` entry the agent's `session/new` config options do not advertise does not fail session creation: the session proceeds on the agent's default, the response carries an `ignored` array (`[{ "feature": "agent.mode"|"agent.model"|"agent.effort"|"agent.config_option", "value", "capability", "reason", "option_id"? }]`, omitted when empty), and a warn-level `session.capability_ignored` session event records the omission. `option_id` is present only on `agent.config_option` records, naming the dropped map entry. A failure from setting an advertised option is still an error.

Session close is history-preserving: the runtime calls ACP `session/close` when supported, marks the local row `closed`, and keeps durable events/query history. Permanent deletion is deferred until product semantics are defined.

`GET /v1/sessions/{id}/changes` reduces explicit ACP `type: "diff"` tool-call content into the latest tool-call snapshot. A missing `oldText` is returned as `null` and represents a created file. Tool-call-update content replaces the prior collection when present and otherwise leaves it intact. The snapshot is bounded, process-local, and identified by `generation` plus `revision`; `truncated: true` means whole tool calls were omitted by a capacity limit. It is not rebuilt from SQLite after restart. Raw `session.update` event persistence and WebSocket delivery are unchanged.

`POST /v1/sessions/{id}/fork` also accepts optional `{ "message_id": "<prompt message id>" }`. `message_id` requires an acknowledged ACP prompt message id from the parent session; unsupported fork capabilities return HTTP 501 `agent.unsupported_capability`.

Prompt status values are `pending`, `running`, `completed`, `errored`, `cancelled`, and `stalled`. `stalled` is a terminal status reached only when the stale-prompt sweeper observes no ACP `session/update` activity for longer than `[prompts].stale_threshold`. From the client's perspective, a `stalled` prompt is final: it will not transition back to `running`, and recovery means submitting a new prompt. See `docs/specs/runtime.md` for the sweeper contract.

`GET /v1/sessions/{id}/snapshot` is the reconnect-bootstrap helper. The response carries:

- `session` — full session row (id, status, agent id, cwd, title, metadata). The durable `status` is `active`, `available`, or `closed`, distinct from the derived `state` below.
- `in_flight_prompts` — prompts currently in `pending` or `running`, capped at 25 (`SNAPSHOT_IN_FLIGHT_PROMPTS_CAP`). Empty when the session is idle. Each entry is the same shape returned by `GET /v1/sessions/{id}/prompts/{prompt_id}`.
- `last_event_id` — the id of the newest persisted session event, or `null` when the session has no events. Acts as a tail cursor for forward catch-up: callers fetch events newer than the snapshot via `GET /v1/sessions/{id}/events?after=last_event_id`, which paginates forward on `(created_at, id)` ascending.
- `recent_events` — the latest session events, newest-first, capped at 50. The cap is enforced by `SNAPSHOT_RECENT_EVENTS_LIMIT` in `src/api/routes/sessions.rs` and is sized to cover one prompt-turn's worth of updates without bloating the response.
- `available_commands` — the agent's last advertised slash-command list, same entry shape as `GET /v1/sessions/{id}/commands`. Empty when nothing has been advertised; may be stale until the agent re-advertises.

The intended reconnect flow is: `GET snapshot` once to recover state, subscribe to `sessions.{id}` over WebSocket, then use `GET events?after=last_event_id` to catch up on any events that landed between the snapshot read and the WebSocket subscribe. For deeper history (older than the 50-event snapshot window), additional pagination is not currently exposed; older events are reachable only through the durable logs endpoints.

Session status defaults to a rolling `8h` activity window and accepts `window=<duration>` from `1m` through `999h`. Each row includes a derived `state`: `idle`, `prompt_sent`, `working`, `permission_required`, `done`, `stopped`, `error`, `cancelled`, `available`, or `closed`. `done` means the latest prompt completed with `stop_reason = "end_turn"`.

Session list filters accept `limit`, time bounds, and range values. Duration suffixes such as `30m`, `12h`, `60d`, `8w`, `6mo`, and `1y` are interpreted relative to request time.

The local Unix-socket router always exposes selected low-risk daemon-backed routes without bearer auth for local `acps` commands, including `GET /v1/sessions`, `GET /v1/sessions/-/status`, metrics summary, WebSocket summaries, and the security diagnostic. Session-tier HTTP routes are also mounted on the local socket but return 404 unless `[local].session_auth = "keyless"` is active. Admin-tier routes, auth rotation, config import, secret mutation, dependency apply, WebSocket disconnects, and WebSocket upgrades are not registered on the local socket.

### Prompt-Path Error Codes

Terminal prompt failures surface through the prompt row's `error_code` and through the matching session-scoped event:

| `error_code`           | HTTP status | Description                                                                            |
| ---------------------- | ----------- | -------------------------------------------------------------------------------------- |
| `agent.inference_5xx`  | 502         | Upstream inference endpoint returned 5xx (or the 529-overloaded variant)               |
| `agent.inference_4xx`  | 424         | Upstream inference endpoint returned 4xx (rate limit, malformed request)               |
| `agent.request_failed` | 502         | Agent rejected the ACP request for a non-inference reason                              |
| `prompt.stalled`       | n/a         | Sweeper-written code on rows it flipped to `stalled`; not surfaced as an HTTP response |

The `agent.inference_*` codes carry a sanitized public message of the form `"inference endpoint returned <status_code> (<reason_category>)"`, where `reason_category` is drawn from a fixed static enum. No URLs, request/response bodies, headers, or secret material reach the API response or the persisted prompt row; see `docs/specs/state-logging.md` for the full taxonomy and event shapes.

## Metrics Summary

`GET /v1/metrics/summary` accepts `since` and `until`, each an RFC 3339 timestamp or a duration suffix (`1h`, `30m`, `2d`); they default to 24h-ago and now, and `since > until` is a `400`. The response also carries a `window` object echoing the resolved `[since, until)` bounds and a `counts` object of ten totals.

`GET /v1/metrics/summary` includes `prompt_failures` so operators can separate upstream inference outages from local runtime failures. The object contains `total`, explicit counters for each `failure_class` (`inference_5xx`, `inference_4xx`, `agent_request`, `vm`, `sqlite`, `daemon`, `agent_process`, `stalled`), `by_class`, and inference event breakdowns by HTTP status code and reason category.

The `api_connections` metrics block includes `request_count`, `average_duration_ms`, existing `by_status` response buckets, and count maps by method, route template, key kind, event source, origin kind, country code, and region code. Any missing or empty grouping key (including an unauthenticated request's key kind) is bucketed under `unknown`. These counts exclude `/v1/ws`, `/v1/health/*`, and `/v1/status*` for public-tier callers; internal `local`-tier calls are still counted. `request_count` is always present (`0` on an empty window).

## Workspace Files

Workspace routes are session-tier. Paths are workspace-relative. The runtime rejects absolute paths, NUL bytes, `..` traversal, symlink escapes, writes through existing symlink targets, and files above `workspace.max_file_bytes`.

| Route                             | Contract                                   |
| --------------------------------- | ------------------------------------------ |
| `GET /v1/workspace`               | returns workspace metadata                 |
| `GET /v1/files?path=...`          | lists directory entries                    |
| `GET /v1/files/content?path=...`  | reads a file as UTF-8 or base64            |
| `PUT /v1/files/content`           | writes a file atomically                   |
| `POST /v1/files/upload`           | uploads one file below `workspace.uploads` |
| `GET /v1/files/download?path=...` | streams raw file bytes                     |
| `DELETE /v1/files?path=...`       | deletes one regular file                   |

`PUT /v1/files/content` takes `{ "path", "encoding", "content" }`, where `encoding` is `utf8` or `base64`. `POST /v1/files/upload` is multipart with required `path` and `file` fields. `GET /v1/files/content` returns `{ ..., "encoding" }` with the same `utf8`/`base64` values, and `GET /v1/files` entries carry a `kind` of `file`, `directory`, `symlink`, or `other`.

`workspace.max_file_bytes` caps reads, writes, uploads, and downloads. Oversized files return `413 workspace.too_large`.

## Commands

Commands are session-tier and mediated by policy.

| Route                           | Contract                         |
| ------------------------------- | -------------------------------- |
| `POST /v1/commands`             | starts or queues a shell command |
| `GET /v1/commands`              | lists command records            |
| `GET /v1/commands/{id}`         | returns one command              |
| `GET /v1/commands/{id}/output`  | returns persisted output chunks  |
| `POST /v1/commands/{id}/cancel` | cancels a running command        |

Request body:

```json
{
  "command": "rg TODO .",
  "cwd": ".",
  "env": { "NAME": "value" },
  "timeout": "10m"
}
```

Command status values are `pending`, `running`, `exited`, `failed`, and `cancelled`. Command records include `last_output_event_id`, `last_output_at`, `last_output_seq`, `output_bytes`, and `last_progress_at` for reconnect and liveness checks, plus `origin` (`operator` for gateway submissions, `acp` for agent-created client terminals) and `session_id` (set on `acp`-origin rows).

Output is persisted up to the configured byte cap and streamed on the command WebSocket topic while the command runs. `GET /v1/commands/{id}/output` accepts `limit`, `after`, and `order=asc|desc` and returns `{ chunks, next_cursor }`. Each chunk is shaped as `{ event_id, created_at, command_id, stream, seq, data }`.

The reconnect flow is: read `GET /v1/commands/{id}`, subscribe to `commands.{id}`, then query `/output?order=asc&after=<last-seen-event-id>` to catch chunks missed between the HTTP read and WebSocket subscribe.

## Permissions

| Route                               | Tier    | Contract                                   |
| ----------------------------------- | ------- | ------------------------------------------ |
| `GET /v1/permissions/pending`       | session | lists pending requests                     |
| `GET /v1/permissions/{id}`          | session | returns a single permission request        |
| `POST /v1/permissions/{id}/approve` | session | approves a request                         |
| `POST /v1/permissions/{id}/deny`    | session | denies a request                           |

`POST /v1/permissions/{id}/approve` takes `{ "option_id": "<id>"?, "reason": "<text>"? }` and `POST /v1/permissions/{id}/deny` takes `{ "reason": "<text>"? }`. For an ACP-source request, `option_id` names one of the ids in the request's `detail.options` and is forwarded to the agent as-is (not validated against that list); omitting it selects the request's first option. A command-source request has no options.

Cancellation is not an HTTP operation: pending requests are cancelled internally when their owning flow ends (session close, mediated-command cancel).

Deciding a request that is already terminal — including approving a permission whose command has since died — returns `409` with `permission.invalid_transition`. Clients should treat it as "the request was already settled" (the decision event names the cause), not as a retryable error.

Permission requests are created by ACP permission callbacks and by mediated commands when policy requires review. Composed mediated commands using shell control operators, command substitution, or process substitution require review before execution, including in `permissions.mode = "auto"`. Policy matching considers shell-word-normalized command words, so constructed spellings such as quoted or escaped command names can be denied or routed to review.

## Dependencies

| Route                 | Tier    | Contract                           |
| --------------------- | ------- | ---------------------------------- |
| `GET /v1/deps`        | session | reports declared dependency status |
| `POST /v1/deps/check` | session | re-checks dependency status        |
| `POST /v1/deps/apply` | admin   | runs declared install actions      |

The runtime never invents package-manager commands. Only install actions declared in config can be applied. System-scope actions escalate through `sudo -n` when the daemon is non-root and passwordless sudo is available; otherwise the `installer_runs` row is recorded as `privilege_required`.

`POST /v1/deps/apply` takes `{ "confirmation": true, "feature": "<name>"? }`. `confirmation` defaults to `false`, which yields a side-effect-free preview (`applied: false`, `candidates` only, no `report`); a confirmed apply installs and returns `report.apply_run_id` for correlating dependency audit rows. `feature` filters candidates. Each result's `outcome.kind` is one of `installed`, `already_present`, `privilege_required`, or `failed`.

## Status, Logs, Metrics, And Security

| Route                        | Tier    | Contract                                         |
| ---------------------------- | ------- | ------------------------------------------------ |
| `GET /v1/status`             | session | returns local status summary                     |
| `GET /v1/status/agent`       | session | alias for agent status                           |
| `GET /v1/status/connections` | session | returns active HTTP request count                |
| `GET /v1/health/live`        | session | process liveness                                 |
| `GET /v1/health/ready`       | session | subsystem readiness summary; `503` when degraded |
| `GET /v1/security/check`     | admin   | runs the self-check, persists the run, returns findings       |
| `GET /v1/security/history`   | admin   | lists persisted self-check runs newest-first                  |
| `GET /v1/security/history/{run_id}` | admin | returns a single self-check run with findings          |
| `GET /v1/logs/events`        | session | returns durable event rows; supports `category=` and `order=` |
| `GET /v1/logs/commands`      | session | returns command history; supports `order=`                    |
| `GET /v1/logs/permissions`   | session | returns permission history; supports `order=`                 |
| `GET /v1/logs/security`      | session | returns security events; `order=` applies to both result streams |
| `GET /v1/logs/sessions`      | session | returns session-scoped history; supports `order=`             |
| `GET /v1/metrics/summary`    | session | returns aggregate metrics for a time window                   |
| `GET /v1/installer/runs`     | session | returns installer run rows; `active=true` returns in-flight steps with server-side elapsed time |

`GET /v1/installer/runs` reads the `installer_runs` table (agent installs and updates, plus `deps_apply` rows). Query parameters: `active=true` returns only in-flight (`running`) steps, oldest first, ignoring `limit`; `agent=<id>` scopes to one agent id (`deps_apply` covers dependency installs); `limit` caps history rows (default 100, max 1000). Each row carries `{ "id", "agent_id", "operation", "step", "method", "status", "started_at", "finished_at", "exit_status", "version" }`; running rows additionally carry `elapsed_seconds`, computed server-side so pollers need no clock sync with the daemon. `operation` is `install` or `update`; `step` is `install`, `harness`, `adapter`, or `deps_apply`; `method` is `shell`, `npm`, `github`, `apt`, or `native`; `status` is `running`, `ran`, `failed`, `error`, `timeout`, `skipped`, `config_error`, `installed`, or `privilege_required`. This is the live-progress surface for harness/adapter installs, which can run for minutes: a platform driving instance init polls `?active=true&agent=<id>` to render step-level progress while `POST /v1/agent/install` (or a switch) is in flight. Step stdout/stderr previews and the on-disk log directory are never returned; logs stay on the host. A row that remains `running` means the daemon died mid-step — treat it as stale, not as progress.

Log query filters are per-route, not one shared set. All accept `limit` (default 100, values above 1000 clamped, not rejected), `since`, `until`, `after`, and `order` (`asc` or `desc`, default `desc`). Beyond those: `/v1/logs/events` adds `level`, `kind`, `source`, `session_id`, `command_id`, `permission_id`, and `category`; `/v1/logs/commands` and `/v1/logs/sessions` add `status`; `/v1/logs/permissions` adds `kind`, `source`, and `permission_id`; `/v1/logs/security` adds `category` plus the per-stream cursors `auth_failures_after` and `events_after` (a shared `after` is only the fallback for each, and because the two streams page independently, reusing one cursor for both mis-pages). On `/v1/logs/security`, `order` applies to both `auth_failures` and `events`; `category` accepts the security-category labels documented in `docs/specs/state-logging.md` and constrains only the `events` stream. `kind` matches by exact value or, with a trailing `.`, by dotted-namespace prefix.

`GET /v1/status` also carries `deps_apply_in_flight`, true while a `POST /v1/deps/apply` is still running its install actions. The apply keeps running after the HTTP client that started it goes away, so this is the only way to observe one. It is advisory: a caller that intends to restart or reconfigure the runtime should wait for it to clear, because a restart signals the process group and tears the install down mid-flight, leaving a half-applied package set and an unfinalized `installer_runs` row.

`GET /v1/status` and `GET /v1/health/live` both carry a `server` object with the running version and the capabilities this build advertises:

```json
{
  "version": "0.1.9",
  "features": ["network-provider-workload-env", "agent-test-json", "managed-credential-base-url"]
}
```

`features` exists because `version` is not a usable capability signal: a nightly build carries its fourth version component only in the git tag, so a nightly with a feature and one without report the same three-part base version. Orchestrators that gate wire calls or config writes on a capability must test membership in `features`; an absent or empty list means none of the listed capabilities are present. The names are a stable contract — `network-provider-workload-env` for `[extensions.<name>.workload_env]`, `agent-test-json` for `acps agent test --format json`, and `managed-credential-base-url` for `base_url` on a managed-state credential selection.

Readiness includes an `mcp` object for configured MCP declarations:

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

Stdio server rows may include `command_path`. Failing rows may include `missing_secret_refs` and `reason`. HTTP MCP readiness validates declaration shape and secret refs only; it does not call the remote MCP endpoint.

Readiness also reports orphaned agent process groups under `agent.orphaned_process_count` and `agent.orphaned_process_pids`. Any live process group from an older `agent.started` lifecycle row, excluding the currently supervised PID, degrades readiness with `agent` in `failing`.

## WebSocket

`GET /v1/ws` upgrades to a WebSocket connection. Clients authenticate with the session key and send a `{ "type": "subscribe", "topics": [...] }` frame to subscribe; frames of any other `type` are ignored. Topics:

- `logs`
- `workspace`
- `permissions`
- `status`
- `commands.{id}`
- `sessions.{id}`
- `agent.lifecycle`

WebSocket management routes:

| Route                                | Tier    | Contract                                     |
| ------------------------------------ | ------- | -------------------------------------------- |
| `GET /v1/ws/connections`             | session | lists active connections without raw secrets |
| `GET /v1/ws/sessions`                | session | lists session-topic subscriptions            |
| `POST /v1/ws/connections/disconnect` | admin   | disconnects the listed connections (`connection_ids`) |
| `POST /v1/ws/sessions/disconnect`    | admin   | disconnects subscribers to the listed sessions (`session_ids`) |

Both disconnect bodies also accept an optional `reason`, recorded on the resulting `ws.client_disconnected` event as `operator_reason` (present only when supplied; the event's `reason` stays the machine cause `operator_disconnect`).

## HTTP Hardening

The API enforces bearer auth, request-size limits, origin checks, rate limits, auth-failure blocking, and bounded proxy-header trust. Disallowed browser origins return `403 auth.origin_not_allowed`. Oversized JSON requests return `413 request.too_large`.
