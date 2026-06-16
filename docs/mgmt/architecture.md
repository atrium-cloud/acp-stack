# Architecture

`acp-stack` is a Rust runtime with one CLI and daemon binary (`acps`) backed by shared runtime modules.

## Runtime Shape

```mermaid
flowchart LR
    Operator["Operator / remote client"] --> API["HTTP + WebSocket API"]
    Local["Local acps views"] --> LocalSocket["internal local socket"]
    API --> Runtime["Runtime services"]
    LocalSocket --> Runtime
    Runtime --> State["SQLite state"]
    Runtime --> Secrets["Encrypted secret store"]
    Runtime --> Workspace["Workspace"]
    Runtime --> Agent["ACP agent process"]
    Agent --> MCP["Configured MCP servers"]
```

## Subsystems

| Subsystem        | Responsibility                                                    |
| ---------------- | ----------------------------------------------------------------- |
| Config           | load, validate, import, export, and canonicalize TOML             |
| Auth             | API key validation, auth tiers, and request envelopes             |
| API              | HTTP routes, WebSocket subscriptions, and client-facing contracts |
| Local listener   | owner-only Unix-socket surface for keyless local `acps` routes    |
| State            | SQLite migrations and repositories for durable runtime records    |
| Secrets          | age-compatible key management and encrypted values                |
| Agent supervisor | process lifecycle for the configured ACP agent                    |
| ACP bridge       | ACP initialization, sessions, prompts, updates, and permissions   |
| Agent switch     | harness migration planning and provider/API-key compatibility     |
| Install catalogs | curated agent registry, Agent Skills source registry, and skills installer |
| Workspace        | bounded file operations and workspace source materialization      |
| Command gateway  | policy-mediated shell command execution and output capture        |
| Permissions      | durable approval, denial, cancellation, and expiry                |
| Dependencies     | declaration checks and explicit install actions                   |
| Logging          | local event history, metrics, and optional external sink          |
| Edge             | reverse-proxy/tunnel artifacts and optional Cloudflare provisioning |

## Boundaries

- `acp-stack` supervises one configured ACP agent per runtime.
- Config is portable and contains references, not secret values.
- SQLite is the local source of truth for runtime history.
- The secret store is the only source for secret values.
- External telemetry sinks consume the same normalized event stream as local SQLite logging.
- Agent behavior stays behind ACP; `acp-stack` owns runtime mediation around it.
- The local socket is allowlisted for low-risk observability plus admin-enabled session-tier HTTP access; public admin APIs are not exposed through it.
- Deployment profiles should not change runtime behavior, only process and edge shape.

## Maintainer Notes

Development and verification guidance lives in [development.md](development.md). Product behavior contracts live under [../specs](../specs).
