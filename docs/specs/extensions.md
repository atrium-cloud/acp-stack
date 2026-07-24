# Extensions

An extension is a typed, data-declared integration seam. The operator declares an instance in the `[extensions]` config table, selecting from the small set of types acp-stack defines; acp-stack supervises or serves each type's generic contract and never learns the extension's semantics. There is no dynamic route registration and no in-process plugin loading — the extension itself is whatever external software fulfills the contract: an executable for `network-provider`, an API client for `managed-state`. Extensions require no particular language and nothing is compiled into acp-stack.

## Declaration

```toml
[extensions.egress]
type = "network-provider"
provider = ["/usr/local/libexec/acps-network-provider", "--config", "/etc/provider.toml"]
provider_timeout = "30s"
provider_stderr = "daemon"

[extensions.platform-state]
type = "managed-state"
capability = "provider-credential"
```

The table key is the operator-chosen instance name: lowercase alphanumeric with interior hyphens, at most 64 bytes, because it becomes an API path segment and a diagnostics label. Each type accepts only its own fields; a field that would enforce nothing for the declared type is rejected at config load. There is no CLI for declaring extensions — imported or directly edited TOML is the only way — and `acps extensions status` reports the declared instances read-only.

## Type `network-provider`

Declaring a `network-provider` instance switches every sandboxed spawn (agent harness and each mediated command alike) to a fresh, per-spawn network namespace whose policy belongs entirely to the external provider executable. Requires `[workspace.sandbox] mode = "unshare"`; at most one instance may be declared. An empty or omitted `provider` argv means deny-all networking.

Fields: `provider` (lifecycle executable argv; the executable must be an absolute path), `provider_timeout` (default `30s`, applied independently to setup and teardown), `provider_stderr` (`daemon` or `null`).

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
      "source_refs": {}
    }
  }
}
```

- `schema_version` must be `1`.
- `revision` is the orchestrator's monotonically increasing registry revision; it must be a positive integer.
- `selection` is a required key that may be `null` (clear the namespace's credential). A missing key is a parse error, never a destructive clear.
- `values` are keyed by env-var name and validated against the provider's canonical env-var contract from the embedded mapping: the canonical API-key env var and every required companion must be present, only contract env vars are accepted, and unknown providers are rejected. The contract check is provider-scoped, matching the catalog's semantics; agent-specific env-var mapping happens at spawn-time resolution.
- `source_refs` may name flat secret-store refs per env var instead of inline values; each ref resolves into the stored values at apply time and the ref name is retained. A ref-backed selection is replay-stable only while the referenced secrets are stable.

Revision semantics, enforced in the store and persisted atomically with the credential catalog swap under the agent-config mutation lock:

- `revision` greater than the applied watermark: applied (or cleared for a `null` selection; the watermark survives a clear).
- `revision` equal to the watermark with identical content: idempotent no-op.
- `revision` equal to the watermark with different content, or lower: rejected with `409` `extensions.revision_conflict`.

Ownership is store-level provenance, not endpoint behavior: a namespace may create catalog entries or replace its own, and nothing else. Applying onto an operator-managed credential or another namespace's credential is rejected with `400` `extensions.state_ownership`; symmetrically, operator credential flows refuse to modify externally-owned entries. An undeclared or non-managed-state `{name}` is `404` `extensions.not_found`. The declared namespace set is resolved from the config the daemon started with, like the rest of runtime config: a config import that adds a `managed-state` instance answers `404` until the next daemon start.

Responses use the standard envelope: `{"ok": true, "data": {"applied_revision": 7, "outcome": "applied"}}` with `outcome` one of `applied`, `cleared`, `noop`. Every apply records a `server.extension_managed_state_applied` audit event carrying the namespace, outcome, revision, and provider id — never credential values.

## Versioning

Both contracts carry an explicit version: the network-provider env contract advertises `ACPS_SANDBOX_NETWORK_PROTOCOL=1`, and the managed-state request schema is gated on `schema_version = 1`. Additions within a version are backward compatible; a breaking contract change increments the version.
