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

Each type accepts only its own fields. A field that is inert for the declared type is rejected at config load. Extensions are declared through TOML alone, whether imported or directly edited. `acps extensions status` reports the declared instances read-only.

## Type `network-provider`

Declaring a `network-provider` instance switches every sandboxed spawn (agent harness and each mediated command alike) to a fresh, per-spawn network namespace. Its policy belongs entirely to the external provider executable. Requires `[workspace.sandbox] mode = "unshare"`. At most one instance may be declared. An empty or omitted `provider` argv means deny-all networking.

Fields: `provider`, `provider_timeout`, `provider_stderr`, `workload_env`. `provider` is the lifecycle executable argv; the executable must be an absolute path. `provider_timeout` defaults to `30s` and applies independently to setup and teardown. `provider_stderr` is `daemon` or `null`. `workload_env` is environment injected into every workload spawned inside the namespace.

A namespace whose policy routes traffic through a proxy or a private CA is usable only when the workload knows to use it. `workload_env` exists for that.

- Entries are injected into the agent harness, mediated commands, and ACP terminals alike, covering everything that runs inside the namespace.
- They are injected into the workload side only. The provider starts from a cleared environment carrying only the `ACPS_SANDBOX_NETWORK_*` contract variables.
- Values are passed outside argv, keeping them out of process listings.

The declaration is applied last, after `[agent].env`, after any runtime-managed launch variables, after a managed [secret environment](#capability-secret-environment), and after `[workspace.sandbox].workload_env`, so it wins on conflict. A name declared both here and in `[workspace.sandbox].workload_env` fails config load.

### `workload_env` Validation

- At most 16 entries.
- Names must match `[A-Za-z_][A-Za-z0-9_]*` and be at most 128 bytes.
- Values must carry at least one byte and at most 16 KiB.
- `PATH` and `HOME` are rejected at config load. Both are runtime-managed at every spawn seam, so a declared value would be silently dropped.

The provider wire contract is specified in [security.md](security.md#network-isolation-unshare-only), and the supervisor mechanics in [runtime.md](runtime.md#network-isolated-spawns). The contract covers the `<executable> setup|teardown <configured-args...>` verbs, the `ACPS_SANDBOX_NETWORK_*` environment variables with protocol version 1, fail-closed setup, process-group supervision, and stderr routing.

## Type `managed-state`

A `managed-state` instance grants an external orchestrator ownership of one named state namespace. Its `capability` names the `desired.kind` family the namespace accepts; a body of another kind is rejected with `request.invalid_param`.

- `provider-credential`: the namespace holds provider credentials, stored in the encrypted secret store's provider credential catalog exactly like operator-managed ones, with the namespace recorded as their provenance source. It takes `kind: "provider-credential"` (at most one selection) and `kind: "provider-credential-set"` (see [Kind `provider-credential-set`](#kind-provider-credential-set)).
- `secret-environment`: the namespace holds workload env vars whose values come from secret-store refs. See [Capability `secret-environment`](#capability-secret-environment).

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
- `selection` is a required key that may be `null` (clear the namespace's credential). A missing key is a parse error; only an explicit `null` clears.
- `values` are keyed by env-var name and validated against the provider's env-var contract.
- `source_refs` may name flat secret-store refs per env var instead of inline values. Each ref resolves into the stored values at apply time, and the ref name is retained. A ref-backed selection is replay-stable only while the referenced secrets are stable.

### The env-var contract for `values`

- For a mapped (registry) provider, the contract comes from the embedded mapping. The canonical API-key env var and every required companion must be present, and only contract env vars are accepted.
- For a provider id outside the mapping, the selection is accepted only when the running agent config declares that id as a custom provider. Its contract is exactly the configured `api_key_ref` as the single env-var key.
- Config validation keeps that contract unambiguous. Registry ids are reserved for mapped providers. Every declaration of one custom provider id (primary agent, subagents, and Array targets alike) must name the same `api_key_ref`.
- A provider id outside both the mapping and the configured custom providers is rejected with `request.invalid_param`.

The contract check is provider-scoped, matching the catalog's semantics. Agent-specific env-var mapping happens at spawn-time resolution.

Rejection happens before any revision watermark persists. An orchestrator that applies a custom-provider credential before init has written the provider config can therefore retry the same revision once the config lands. The handler re-reads the config from disk on every apply, leniently. An unusable MCP server or skill-source declaration is skipped the way it is at daemon start, and the credential rotation proceeds.

### After an apply

The native config is rewritten immediately after the store write, under the mutation lock the apply handler already holds. The agent reads it at process start, and the orchestrator restarts the agent after a credential push. Rewriting also runs on a `noop` replay, so a retry after a failed native-config write still settles it. A set whose Agent launch env does not resolve after the store write keeps its outcome and answers `agent_environment: "restart_required"`, and the failure is logged by name.

For each provider whose credential the apply added, removed or changed, the provider model-catalog cache entries are invalidated, together with the entries of every other id and endpoint of its provider row. The next catalog read refetches at the declared `models_url` with the credential now in force. Only providers that declare a `models_url` are ever fetched.

A selection names no endpoint. The agent and the catalog fetch dial the provider's own host, so a sealed value in `values` is redeemed by the network provider that intercepts that host.

### Kind `provider-credential-set`

A namespace declared with `capability = "provider-credential"` also takes credentials for several providers in one revision:

```json
{
  "schema_version": 1,
  "revision": 8,
  "desired": {
    "kind": "provider-credential-set",
    "selections": [
      { "provider_id": "openrouter", "values": { "OPENROUTER_API_KEY": "sk-or-..." }, "source_refs": {} },
      { "provider_id": "deepseek", "values": { "DEEPSEEK_API_KEY": "sk-..." }, "source_refs": {} }
    ]
  }
}
```

- `selections` holds 1 to 16 entries with distinct `provider_id`, each validated as a `provider-credential` selection. After the apply the namespace owns exactly these providers' credentials; one it held before and the set leaves out is removed. A credential whose values are unchanged keeps its stored revision.
- The selections become the Agent's `[agent.providers].active`, in order, written in the same call under the same lock. The `[agent.provider]` default slot stays while its provider is in the set; otherwise it moves to the first provider with its model cleared. Alias selections of providers outside the set are dropped, and a `[agent.subagent.provider]` outside the set is cleared, so the subagent runs on the default slot. With a custom provider in the default or subagent slot, the credentials are stored and the Agent config is left as it is.
- The candidate config is validated before anything is stored, so a set the Agent cannot run (more than one provider on an agent without multiple active providers, a provider the agent does not support, two selections with different values for one env var such as `opencode` and `opencode-go`) is refused with no watermark written.
- The config write and the native config rewrite run on a `noop` replay too, so a replay converges after an interrupted write. The write restarts nothing.
- The answer adds `agent_environment`: `live` when no Agent runs or the running Agent's loaded providers match the applied set, and `restart_required` when the running Agent must restart to take it. A replay computes it again.

```json
{ "ok": true, "data": { "applied_revision": 8, "outcome": "applied", "agent_environment": "restart_required" } }
```

The two credential kinds share the namespace's record. A one-selection set at the applied revision of a `provider-credential` body carrying the same selection replays as `noop`, and the other way round. A `provider-credential` body at a higher revision over a set removes every provider of the set it leaves out, from the store and from `[agent.providers].active`. When nothing else remains active, its selection becomes the active set as a one-selection set does, and a `null` selection drops the `[agent.providers]` table and leaves the default slot as it is.

### Capability `secret-environment`

Request body:

```json
{
  "schema_version": 1,
  "revision": 7,
  "desired": {
    "kind": "secret-environment",
    "entries": { "SEARCH_API_KEY": "SEARCH_API_KEY_REF" }
  }
}
```

- `entries` maps each workload env var name to the flat secret-store ref whose value it carries. It is a required key; an empty map clears the namespace.
- At most 128 entries. Names and refs match `[A-Za-z_][A-Za-z0-9_]*` and are at most 128 bytes.
- Every ref must be in the secret store at apply time.
- A name is rejected when a source that writes it at every spawn already sets it: the runtime (`PATH`, `HOME`, `ACPS_*`, and the runtime-managed launch variables), a network-provider `workload_env`, or another `secret-environment` namespace. Every other source meets the namespace in the spawn precedence below, so an apply never depends on agent config or the selected provider.

The namespace stores the refs, never their values. Every workload spawn (the agent process, ACP terminals, and mediated commands) reads the current value of each ref, so a secret rotated under the same ref reaches the next spawn without a new revision; the running agent process keeps the value it started with until the orchestrator restarts it. The values fill beneath the spawn's own env and above `[workspace.sandbox].workload_env`, and the network-provider declaration still lands last. In the agent process, an `[agent].env` entry or the selected provider's credential variable of the same name therefore wins; terminals and mediated commands carry the namespace's value. Deleting a secret an entry still names is refused with `409` `secrets.referenced`, so the namespace drops the entry before its secret goes. A ref that is missing anyway fails the spawn, naming the namespace, the variable, and the ref: the agent start answers `409` `extensions.environment_unresolved`, and a terminal or mediated command records the same reason on its failed spawn.

### Revision semantics and ownership

Revision semantics are enforced in the store and persisted atomically with the credential catalog swap under the agent-config mutation lock:

- `revision` greater than the applied watermark: applied (or cleared for a `null` selection or empty `entries`; the watermark survives a clear).
- `revision` equal to the watermark with identical content: idempotent no-op. A `secret-environment` replay compares the entries' refs, not their values.
- `revision` equal to the watermark with different content, or lower: rejected with `409` `extensions.revision_conflict`.

Ownership is store-level provenance, distinct from endpoint behavior. A namespace may only create catalog entries or replace its own.

- Applying onto an operator-managed credential or another namespace's credential is rejected with `400` `extensions.state_ownership`.
- Symmetrically, operator credential flows refuse to modify externally-owned entries.
- An undeclared `{name}`, or one belonging to another extension type, is `404` `extensions.not_found`.

The declared namespace set is resolved from the config the daemon started with, like the rest of runtime config. A config import that adds a `managed-state` instance answers `404` until the next daemon start.

Responses use the standard envelope: `{"ok": true, "data": {"applied_revision": 7, "outcome": "applied"}}` with `outcome` one of `applied`, `cleared`, `noop`. Every apply records a `server.extension_managed_state_applied` audit event carrying the namespace, kind, outcome, revision, provider ids, and environment variable names only.

## Versioning

Both contracts carry an explicit version: the network-provider env contract advertises `ACPS_SANDBOX_NETWORK_PROTOCOL=1`, and the managed-state request schema is gated on `schema_version = 1`. Additions within a version are backward compatible; a breaking contract change increments the version.
