# acpctl Spec

`acpctl` is the local, agent-facing control interface for runtime inspection and constrained local operations. It shares core services with the daemon and HTTP API instead of creating a separate behavior path.

## Local Agent CLI

`acpctl` is the local, agent-facing control CLI installed on the instance. It exposes a constrained subset of runtime operations for agents and local shell users without requiring the public HTTP session/admin API keys.

Example commands:

```sh
acpctl status
acpctl security check
acpctl deps check
acpctl logs query --since 1h
acpctl workspace list .
acpctl workspace read README.md
acpctl command run "rg TODO ."
acpctl config export
acpctl permissions pending
acpctl mcp serve
```

`acpctl` should call the same core service layer as the daemon and HTTP API. It is not a separate implementation path.

## Local Introspection Interface

0.0.3 adds `acpctl`, a local interface intended for agents running inside the instance.

The local interface exists so an agent can inspect and operate on the runtime safely:

- check runtime status
- inspect dependency status
- query recent logs
- run the security self-check
- list/read/write workspace files
- run mediated shell commands
- export config with secret references only
- inspect pending permission requests
- expose the same surface as a local MCP server through `acpctl mcp serve`

Security boundary:

- `acpctl` runs as the unprivileged runtime user.
- `acpctl` uses a local capability mechanism, such as a Unix socket or owner-only local token, not the public session/admin API keys.
- all `acpctl` actions go through the same permission pipeline as HTTP-triggered actions.
- all `acpctl` actions are logged with source `local`.
- agents cannot read secret values through `acpctl`.
- agents cannot rotate API keys through `acpctl`.
- agents cannot disable permissions, rate limits, CORS/origin checks, or security logging.
- agents cannot approve their own high-risk command requests unless policy explicitly allows it.
- config export through `acpctl` follows normal export rules and includes secret references only.

## Implementation (0.0.3)

The 0.0.3 implementation realizes this surface as a `tokio` Unix-domain-socket listener inside the `acps` daemon (`src/local_listener.rs`, with router/socket internals under `src/local_listener/`). When `acps serve` starts it binds the socket at `~/.local/share/acp-stack/acpctl.sock` (override with `[acpctl] socket_path` in the TOML), sets the file mode to `0600` inside a `0700` parent directory, and unlinks the socket on graceful shutdown. The listener serves an Axum router that mounts an explicit allowlist of the ten operations above; any other route returns 404, including the public-API routes for secret values, API-key rotation, permission approve/deny, config import, and agent install/start/stop. A `tag_local` middleware stamps every UDS request with `KeyKind::Local` so the public router's tier gate rejects the tag if it ever leaks across listeners, and reused handlers attribute durable writes (`api.request`, `workspace.write`, etc.) to `source = "local"`. The `acpctl` binary entrypoint (`src/bin/acpctl/main.rs`) speaks HTTP/1.1 over the socket through helper modules under `src/bin/acpctl/` and forwards each subcommand to its mapped route; it sends no `Authorization` header — filesystem permissions on the socket are the access control.

The optional `acpctl mcp serve` command exposes the local introspection interface as an MCP server for agents that prefer tool calls over shell commands. The MCP server must enforce the same capability, permission, and logging rules as the CLI.
