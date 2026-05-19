# acp-stack

`acp-stack` is a self-hostable Linux runtime for ACP-compatible agents. It provides the runtime layer around an agent: config, secrets, workspace access, MCP wiring, permissions, logs, and an HTTP/WebSocket API.

## Agent Support

Last updated: May 18, 2026

- OpenCode (native)
- Pi Agent via `pi-acp` (adapter)
- Amp Code via `amp-acp` (adapter)
- Cursor CLI (native)
- Goose (native)
- Codex CLI via `codex-acp` (adapter)

See [docs/specs/agents/support.md](docs/specs/agents/support.md) for the support policy, verification process, and currently unsupported agents.

## Deployment

See [docs/deploy/docker.md](docs/deploy/docker.md) for Docker deployment, [docs/deploy/systemd.md](docs/deploy/systemd.md) for systemd deployment, and [docs/deploy/cloudflare.md](docs/deploy/cloudflare.md), [docs/deploy/nginx.md](docs/deploy/nginx.md), or [docs/deploy/caddy.md](docs/deploy/caddy.md) for public-edge reverse proxy guidance.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidelines.

## License

`acp-stack` is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
