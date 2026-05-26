# acp-stack

`acp-stack` is a self-hostable Linux runtime for ACP-compatible agents. It wraps an agent with durable config, encrypted secrets, workspace access, MCP server wiring, command mediation, permission review, logs, and an HTTP/WebSocket API.

## Deployment

- Docker deployment: [docs/deploy/docker.md](docs/deploy/docker.md)
- systemd deployment: [docs/deploy/systemd.md](docs/deploy/systemd.md)
- Public edge options: [Cloudflare](docs/deploy/cloudflare.md), [Nginx](docs/deploy/nginx.md), or [Caddy](docs/deploy/caddy.md)
- CLI contract: [docs/specs/cli.md](docs/specs/cli.md)
- Config contract: [docs/specs/config.md](docs/specs/config.md)
- API contract: [docs/specs/api/api.md](docs/specs/api/api.md)

## Supported Agents

These harnesses are currently supported:

| Agent      | ACP Compat |
| ---------- | ---------- |
| OpenCode   | native     |
| Pi Agent   | adapter    |
| Amp Code   | adapter    |
| Cursor CLI | native     |
| Goose      | native     |
| Codex CLI  | adapter    |

"native" means the agent harness is compatible with ACP natively; "adapter" means an ACP adapter must be installed alongside the harness to enable agent-ACP communication.

- Agent setup notes live under [docs/agents](docs/agents/).
- The support policy and current unsupported list are in [docs/specs/agents/support.md](docs/specs/agents/support.md).

## Agent Harness Authentication

To deploy agents with `acp-stack`, you must use a supported API key env var for the harness of your choice. See [docs/specs/agents/api_key.md](docs/specs/agents/api_key.md) for more details.

Note:
- `acp-stack` supports only API key-based authentication.
- `acp-stack` does not support the use of OAuth tokens, even if the underlying harness supports it.
- `acp-stack` also does not support browser login since this is intended for headless deployment.

## Contributing

External PR contributions are paused for the foreseeable future because maintainers do not currently have enough review capacity to handle them responsibly. Only verified collaborators can contribute directly to this repo at this time.

Issues and security reports remain welcome. Anyone who wants to build on the project can fork it under the Apache 2.0 license. See [CONTRIBUTING.md](CONTRIBUTING.md) for details.

## License

`acp-stack` is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
