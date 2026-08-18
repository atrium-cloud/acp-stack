# Extensions

An extension is a typed, data-declared integration seam. The operator declares an instance in the `[extensions]` config table, selecting from the small set of types acp-stack defines; acp-stack supervises or serves each type's generic contract and never learns the extension's semantics. There is no dynamic route registration and no in-process plugin loading — the extension itself is whatever external software fulfills the contract: an executable for `network-provider`, an API client for `managed-state`. Extensions require no particular language and nothing is compiled into acp-stack.

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

The table key is the operator-chosen instance name: lowercase alphanumeric with interior hyphens, at most 64 bytes, because it becomes an API path segment and a diagnostics label. Each type accepts only its own fields; a field that would enforce nothing for the declared type is rejected at config load. There is no CLI for declaring extensions — imported or directly edited TOML is the only way — and `acps extensions status` reports the declared instances read-only.

## Type `network-provider`

Declaring a `network-provider` instance switches every sandboxed spawn (agent harness and each mediated command alike) to a fresh, per-spawn network namespace whose policy belongs entirely to the external provider executable. Requires `[workspace.sandbox] mode = "unshare"`; at most one instance may be declared. An empty or omitted `provider` argv means deny-all networking.

Fields: `provider` (lifecycle executable argv; the executable must be an absolute path), `provider_timeout` (default `30s`, applied independently to setup and teardown), `provider_stderr` (`daemon` or `null`), `workload_env` (environment injected into every workload spawned inside the namespace).

`workload_env` exists because a namespace whose policy routes traffic through a proxy or a private CA is only usable if the workload knows to use it. Entries are injected into the agent harness, mediated commands, and ACP terminals alike — everything that runs inside the namespace — and never into the provider executable itself, which starts from a cleared environment carrying only the `ACPS_SANDBOX_NETWORK_*` contract variables. Values never appear in argv, so they are not visible in a process listing.

The declaration is applied last, after `[agent].env` and after any runtime-managed launch variables, so it wins on conflict. That precedence is deliberate: the declaration is infrastructure config from the operator who owns the namespace, and a workload whose egress environment is half-overridden reaches no network at all. At most 16 entries; names must match `[A-Za-z_][A-Za-z0-9_]*` and be at most 128 bytes; values must be non-empty and at most 16 KiB. `PATH` and `HOME` are rejected at config load because both are runtime-managed at every spawn seam and a declared value would be silently dropped.

The provider wire contract — `<executable> setup|teardown <configured-args...>` verbs, the `ACPS_SANDBOX_NETWORK_*` environment variables with protocol version 1, fail-closed setup, process-group supervision, and stderr routing — is specified in [security.md](security.md#network-isolation-unshare-only), and the supervisor mechanics in [runtime.md](runtime.md#network-isolated-spawns).

## Type `managed-state`

A `managed-state` instance grants an external orchestrator ownership of one named state namespace. The only capability today is `provider-credential`: the namespace holds at most one provider credential selection, stored in the encrypted secret store's provider credential catalog exactly like an operator-managed credential, but marked with the namespace as its provenance source.

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
- `selection` is a required key that may be `null` (clear the namespace's credential). A missing key is a parse error, never a destructive clear.
- `values` are keyed by env-var name and validated against the provider's env-var contract. For a mapped (registry) provider the contract comes from the embedded mapping: the canonical API-key env var and every required companion must be present, and only contract env vars are accepted. For a provider id outside the mapping, the selection is accepted only when the running agent config declares that id as a custom provider; its contract is exactly the configured `api_key_ref` as the single env-var key, which config validation keeps unambiguous by reserving registry ids for mapped providers and requiring every declaration of one custom provider id — primary agent, subagents, and Array targets alike — to name the same `api_key_ref`. A provider id that is neither mapped nor configured as a custom provider is rejected with `request.invalid_param`. The contract check is provider-scoped, matching the catalog's semantics; agent-specific env-var mapping happens at spawn-time resolution. Because rejection happens before any revision watermark persists, an orchestrator that applies a custom-provider credential before init has written the provider config can retry the same revision once the config lands. The handler re-reads the config from disk on every apply, leniently: an unusable MCP server or skill-source declaration is skipped the way it is at daemon start rather than blocking a credential rotation.
- `source_refs` may name flat secret-store refs per env var instead of inline values; each ref resolves into the stored values at apply time and the ref name is retained. A ref-backed selection is replay-stable only while the referenced secrets are stable.
- `base_url` is optional and routes this provider's traffic at the given endpoint instead of its vendor default. It must be an `https://` URL with a host, or `http://` to a loopback host (`127.0.0.1`, `::1`, `localhost`) — the same rule MCP HTTP servers obey, so a local relay listener is reachable without weakening the no-plaintext-off-host rule. It must carry no embedded credentials and no query string or fragment, and is at most 2048 bytes. The value is stored verbatim; each agent appends to it per its own native convention. `base_url` participates in the identical-replay comparison, so a replay at the applied revision carrying a different endpoint conflicts rather than silently no-oping.

An endpoint override is written into the configured agent's own native config, so it is accepted only for agents whose registry entry declares `set_provider_base_url`: `opencode` (`provider.<id>.options.baseURL`), `pi` (a `models.json` provider entry carrying only `baseUrl`, which pi treats as an override of the built-in provider and keeps its model list), `codex` (`[model_providers.<id>].base_url`), and `claude-code` (`ANTHROPIC_BASE_URL` in the settings `env` block). Any other agent is rejected with `request.invalid_param` before the revision persists. Codex additionally refuses an override for its built-in `openai` provider: Codex reserves that provider id and the shape a replacement table must take is version-dependent, so `openrouter` or a custom provider is required instead.

The native config is rewritten immediately after the store write, under the mutation lock the apply handler already holds, because the agent reads it at process start and the orchestrator restarts the agent after a credential push. Rewriting also runs on a `noop` replay, so a retry after a failed native-config write heals rather than replaying as a no-op with the endpoint permanently unapplied. Clearing the selection, or applying one without `base_url`, re-provisions without the endpoint and restores the vendor default. `acps agent switch` to an agent that does not declare `set_provider_base_url` is rejected while an override is stored, rather than silently dropping a live routing decision. Provider model-catalog fetches do not follow the override in this version; they still address the vendor host.

Revision semantics, enforced in the store and persisted atomically with the credential catalog swap under the agent-config mutation lock:

- `revision` greater than the applied watermark: applied (or cleared for a `null` selection; the watermark survives a clear).
- `revision` equal to the watermark with identical content: idempotent no-op.
- `revision` equal to the watermark with different content, or lower: rejected with `409` `extensions.revision_conflict`.

Ownership is store-level provenance, not endpoint behavior: a namespace may create catalog entries or replace its own, and nothing else. Applying onto an operator-managed credential or another namespace's credential is rejected with `400` `extensions.state_ownership`; symmetrically, operator credential flows refuse to modify externally-owned entries. An undeclared or non-managed-state `{name}` is `404` `extensions.not_found`. The declared namespace set is resolved from the config the daemon started with, like the rest of runtime config: a config import that adds a `managed-state` instance answers `404` until the next daemon start.

Responses use the standard envelope: `{"ok": true, "data": {"applied_revision": 7, "outcome": "applied"}}` with `outcome` one of `applied`, `cleared`, `noop`. Every apply records a `server.extension_managed_state_applied` audit event carrying the namespace, outcome, revision, and provider id — never credential values.

## Versioning

Both contracts carry an explicit version: the network-provider env contract advertises `ACPS_SANDBOX_NETWORK_PROTOCOL=1`, and the managed-state request schema is gated on `schema_version = 1`. Additions within a version are backward compatible; a breaking contract change increments the version.
