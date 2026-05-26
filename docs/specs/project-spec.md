# Project Spec

`acp-stack` is a standalone Linux runtime for ACP-compatible agents. It is distributed as a Rust binary plus CLIs and is designed to be self-hosted.

## Product Shape

- `acps`: operator CLI and daemon entry point.
- `acpctl`: constrained local interface for agents and local shell users.
- HTTP/WebSocket API: remote client interface.
- ACP bridge: client-side ACP connection to the configured agent.
- SQLite state: durable local history and operational records.
- age-compatible secret store: encrypted local secret values.
- TOML config: portable instance configuration.

## Runtime Boundaries

`acp-stack` owns the runtime boundary around the agent:

- config validation and import/export
- encrypted secret injection
- workspace path policy and file transfer
- mediated shell commands
- ACP session lifecycle and live event fanout
- permission requests and decisions
- MCP server declaration and attachment
- status, logs, metrics, and security self-checks

The configured agent owns model behavior, prompt interpretation, and tool use inside the ACP protocol boundary.

## Initial Release Goals

The initial release focuses on:

- local daemon and CLI operation
- supported headless ACP agents
- Docker and systemd deployment
- Cloudflare/Nginx/Caddy public-edge guidance
- encrypted secrets and two-tier API keys
- workspace files and mediated commands
- durable logs, metrics, and session history
- local `acpctl` and MCP introspection

Out of scope for the initial release:

- multi-tenant hosting
- browser OAuth brokering for agents
- hosted control plane
- GPU scheduling
- snapshot/hibernation management
- automatic package-manager inference

## Detailed Specs

- [CLI](cli.md)
- [Config](config.md)
- [API](api/api.md)
- [Runtime](runtime.md)
- [Security](security.md)
- [State and logging](state-logging.md)
- [ACP bridge](acp/acp-bridge.md)
- [Agent support](agents/support.md)
- [Agent provider config](agents/config.md)
- [MCP example](mcp.md)
