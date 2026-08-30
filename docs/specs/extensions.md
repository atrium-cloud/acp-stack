# Extensions

An extension is a typed, data-declared integration seam. The operator declares an instance in the `[extensions]` config table, selecting from the small set of types acp-stack defines. acp-stack serves each type's generic contract; semantics stay with the extension. Route registration is static, and plugin code runs only in external processes.

The extension itself is whatever external software fulfills the contract: an executable for `network-provider`, an API client for `managed-state`. Extensions may use any language, and extension code stays out of the acp-stack binary.

## Declaration

```toml
[extensions.egress]
type = "network-provider"
provider = ["/usr/local/libexec/acps-network-provider", "--config", "/etc/provider.toml"]
provider_timeout = "30s"
provider_stderr = "daemon"

[extensions.egress.workload_env]
HTTPS_PROXY = "http://127.0.0.1:3128"
NO_PROXY = "localhost,127.0.0.1"

[extensions.platform-state]
type = "managed-state"
capability = "provider-credential"
```

The table key is the operator-chosen instance name: lowercase alphanumeric with interior hyphens, at most 64 bytes. It becomes an API path segment and a diagnostics label.

Each type accepts only its own fields. A field that is inert for the declared type is rejected at config load. Extensions are declared through TOML alone — imported or directly edited. `acps extensions status` reports the declared instances read-only.

## Type `network-provider`

Declaring a `network-provider` instance switches every sandboxed spawn — agent harness and each mediated command alike — to a fresh, per-spawn network namespace. Its policy belongs entirely to the external provider executable. Requires `[workspace.sandbox] mode = "unshare"`. At most one instance may be declared. An empty or omitted `provider` argv means deny-all networking.

Fields: `provider`, `provider_timeout`, `provider_stderr`, `workload_env`. `provider` is the lifecycle executable argv; the executable must be an absolute path. `provider_timeout` defaults to `30s` and applies independently to setup and teardown. `provider_stderr` is `daemon` or `null`. `workload_env` is environment injected into every workload spawned inside the namespace.

A namespace whose policy routes traffic through a proxy or a private CA is usable only when the workload knows to use it. `workload_env` exists for that.

- Entries are injected into the agent harness, mediated commands, and ACP terminals alike — everything that runs inside the namespace.
- They are injected into the workload side only. The provider starts from a cleared environment carrying only the `ACPS_SANDBOX_NETWORK_*` contract variables.
- Values are passed outside argv, keeping them out of process listings.

The declaration is applied last, after `[agent].env` and after any runtime-managed launch variables, so it wins on conflict.

### `workload_env` Validation

- At most 16 entries.
- Names must match `[A-Za-z_][A-Za-z0-9_]*` and be at most 128 bytes.
- Values must carry at least one byte and at most 16 KiB.
- `PATH` and `HOME` are rejected at config load. Both are runtime-managed at every spawn seam, so a declared value would be silently dropped.

The provider wire contract is specified in [security.md](security.md#network-isolation-unshare-only), and the supervisor mechanics in [runtime.md](runtime.md#network-isolated-spawns). The contract covers the `<executable> setup|teardown <configured-args...>` verbs, the `ACPS_SANDBOX_NETWORK_*` environment variables with protocol version 1, fail-closed setup, process-group supervision, and stderr routing.

## Type `managed-state`

A `managed-state` instance grants an external orchestrator ownership of one named state namespace. The only capability today is `provider-credential`. The namespace holds at most one provider credential selection. It is stored in the encrypted secret store's provider credential catalog, exactly like an operator-managed credential. The namespace is recorded as its provenance source.

The seam is one fixed admin route, parameterized by the declared instance name:

```
POST /v1/admin/extensions/{name}/apply
```

Request body:

```json
{
  "schema_version": 1,
  "revision": 7,
  "desired": {
    "kind": "provider-credential",
    "selection": {
      "provider_id": "openai",
      "values": { "OPENAI_API_KEY": "sk-..." },
      "source_refs": {},
      "base_url": "http://127.0.0.1:3129/openai"
    }
  }
}
```

- `schema_version` must be `1`.
- `revision` is the orchestrator's monotonically increasing registry revision; it must be a positive integer.
- `selection` is a required key that may be `null` (clear the namespace's credential). A missing key is a parse error; only an explicit `null` clears.
- `values` are keyed by env-var name and validated against the provider's env-var contract.
- `source_refs` may name flat secret-store refs per env var instead of inline values. Each ref resolves into the stored values at apply time, and the ref name is retained. A ref-backed selection is replay-stable only while the referenced secrets are stable.
- `base_url` is optional and routes this provider's traffic at the given endpoint instead of its vendor default. See [Endpoint overrides](#endpoint-overrides) below.

### The env-var contract for `values`

- For a mapped (registry) provider, the contract comes from the embedded mapping. The canonical API-key env var and every required companion must be present, and only contract env vars are accepted.
- For a provider id outside the mapping, the selection is accepted only when the running agent config declares that id as a custom provider. Its contract is exactly the configured `api_key_ref` as the single env-var key.
- Config validation keeps that contract unambiguous. Registry ids are reserved for mapped providers. Every declaration of one custom provider id — primary agent, subagents, and Array targets alike — must name the same `api_key_ref`.
- A provider id outside both the mapping and the configured custom providers is rejected with `request.invalid_param`.

The contract check is provider-scoped, matching the catalog's semantics. Agent-specific env-var mapping happens at spawn-time resolution.

Rejection happens before any revision watermark persists. An orchestrator that applies a custom-provider credential before init has written the provider config can therefore retry the same revision once the config lands. The handler re-reads the config from disk on every apply, leniently. An unusable MCP server or skill-source declaration is skipped the way it is at daemon start, and the credential rotation proceeds.

### Endpoint overrides

The `base_url` value is an origin: scheme, host, and optional port, with no path. It must be an `https://` URL with a host, or `http://` to a loopback host (`127.0.0.1`, `::1`, `localhost`). This is the same rule MCP HTTP servers obey, so a local relay listener stays reachable while the no-plaintext-off-host rule holds. Embedded credentials, query strings, fragments, and any path are rejected with `request.invalid_param`; the value is at most 2048 bytes.

A stored value carrying a path is rejected by every provisioning path (init, `acps agent set`, `acps agent switch`, and extension apply) until the selection is re-applied with the bare origin; the vendor path is composed by the runtime, so a stored `http://127.0.0.1:3129/v1` becomes `http://127.0.0.1:3129`.

The origin replaces the scheme, host, and port of the vendor base the agent×provider pair would otherwise use, and that base's path is kept verbatim. Claude Code on `moonshotai` writes `http://127.0.0.1:3129/anthropic`; Claude Code on `kimi-coding-plan` writes `http://127.0.0.1:3129/coding/`; codex on `openrouter` writes `http://127.0.0.1:3129/api/v1`; pi and opencode on `opencode-go` write `http://127.0.0.1:3129/zen/go/v1`. `base_url` participates in the identical-replay comparison, so a replay at the applied revision carrying a different origin raises a conflict.

The vendor base comes from the provider row in `data/providers.toml` (`base_url`, with `[providers.base_urls]` per-agent entries where an agent's client expects a different path), from the Claude Code profile, from the Kimi lane, or from a custom provider's own `base_url`. A row base may carry `{ENV_VAR}` placeholders naming the provider's companion env vars; they are filled from the stored credential's companion values when the override is composed, so Cloudflare AI Gateway on opencode writes `http://127.0.0.1:3129/v1/<CLOUDFLARE_ACCOUNT_ID>/<CLOUDFLARE_GATEWAY_ID>/compat`.

An endpoint override is written into the configured agent's own native config or launch environment. It is accepted only for agents whose registry entry declares `set_provider_base_url`:

- `opencode`: `provider.<id>.options.baseURL`.
- `pi`: a `models.json` provider entry carrying only `baseUrl`. pi treats it as an override of the built-in provider and keeps its model list.
- `codex`: `[model_providers.<id>].base_url`.
- `claude-code`: `ANTHROPIC_BASE_URL` in the settings `env` block.
- `hermes`: the base-URL env var the provider's native overlay declares (`OPENROUTER_BASE_URL`, `GLM_BASE_URL`, and the rest of the table in `src/runtime/agent/provider_keys.rs`), exported into the launch environment; `model.provider` keeps the provider-native id. A custom provider, or a mapped provider whose overlay declares no such variable, falls back to a managed named entry in the `providers:` map of `~/.hermes/config.yaml`: `providers.acps-managed` carries the provider's `name`, the rerouted `base_url`, a `key_env` that reads the stored key unconditionally, and a `transport` pinning the wire shape, paired with `model.provider: custom:acps-managed`. `model.base_url` stays untouched on either lane. The bare `custom` lane resolves its key and wire shape from URL heuristics, and both miss on a loopback relay base.
- `goose`: the provider's host setting in `~/.config/goose/config.yaml` (`OPENAI_HOST`, `ANTHROPIC_HOST`, `OPENROUTER_HOST`, `XAI_HOST`). Goose appends its own request path, so the value is the bare origin.
- `kimi`: `KIMI_MODEL_BASE_URL` in the launch environment, the lane's base behind the origin.
- `kilo`: `provider.<id>.options.baseURL` in `~/.config/kilo/kilo.json` for the provider the override names.
- `antigravity`: `GOOGLE_GEMINI_BASE_URL` in the launch environment, the bare origin.

`amp` reaches its own backend over a websocket and is rejected with `request.invalid_param` before the revision persists.

Pairs that have nowhere to write the override, or whose vendor base is unknown, are rejected the same way: codex with its built-in `openai` provider (Codex reserves that id and the replacement table shape is version-dependent; use `openrouter` or a custom provider), hermes providers without a declared `api_mode`, goose providers without a host setting (`mistral`, `groq`, `cerebras`), Claude Code native-auth lanes (Bedrock, Vertex, Foundry), a mapped provider the configured agent does not run, and any mapped provider whose row declares no `base_url` for the agent. An agent switch that would land the overridden provider on such a pair is rejected at plan time.

At most one provider may hold an endpoint override at a time — the native config carries exactly one. A `base_url` selection naming a different provider while another namespace's credential already carries an endpoint is rejected with `request.invalid_param` before the revision persists. The revision stays reusable once the first namespace's endpoint is cleared.

The native config is rewritten immediately after the store write, under the mutation lock the apply handler already holds. The agent reads it at process start, and the orchestrator restarts the agent after a credential push.

Rewriting also runs on a `noop` replay, so a retry after a failed native-config write still applies the endpoint. Clearing the selection, or applying a selection that omits `base_url`, re-provisions with the endpoint removed and restores the vendor default.

While an override is stored, every agent-change path that would strand it is rejected, keeping the live routing decision intact:

- `acps agent switch` to an agent whose registry entry omits `set_provider_base_url` — rejected at plan time.
- Selecting an existing Array target whose agent omits it.
- Re-running init toward such an agent, or toward any custom agent. A custom agent's endpoint surface is entirely self-managed.
- A switch or init re-confirm whose target provider is the overridden one is rejected the same way. This covers pairs that refuse overrides (codex + `openai`).

On an `applied` or `cleared` outcome, the provider model-catalog cache entries for the outgoing and incoming providers are invalidated. The next catalog read refetches from the endpoint now in force.

Provider model-catalog fetches follow the override. When an override names the configured provider, the declared `models_url` is read behind the override origin with its path kept, and the stored value still goes along as the credential.

Only providers that declare a `models_url` are ever fetched. With the override cleared, the declared URL is used unchanged.

### Revision semantics and ownership

Revision semantics are enforced in the store and persisted atomically with the credential catalog swap under the agent-config mutation lock:

- `revision` greater than the applied watermark: applied (or cleared for a `null` selection; the watermark survives a clear).
- `revision` equal to the watermark with identical content: idempotent no-op.
- `revision` equal to the watermark with different content, or lower: rejected with `409` `extensions.revision_conflict`.

Ownership is store-level provenance, distinct from endpoint behavior. A namespace may only create catalog entries or replace its own.

- Applying onto an operator-managed credential or another namespace's credential is rejected with `400` `extensions.state_ownership`.
- Symmetrically, operator credential flows refuse to modify externally-owned entries.
- An undeclared `{name}`, or one belonging to another extension type, is `404` `extensions.not_found`.

The declared namespace set is resolved from the config the daemon started with, like the rest of runtime config. A config import that adds a `managed-state` instance answers `404` until the next daemon start.

Responses use the standard envelope: `{"ok": true, "data": {"applied_revision": 7, "outcome": "applied"}}` with `outcome` one of `applied`, `cleared`, `noop`. Every apply records a `server.extension_managed_state_applied` audit event carrying the namespace, outcome, revision, and provider id only.

## Versioning

Both contracts carry an explicit version: the network-provider env contract advertises `ACPS_SANDBOX_NETWORK_PROTOCOL=1`, and the managed-state request schema is gated on `schema_version = 1`. Additions within a version are backward compatible; a breaking contract change increments the version.
