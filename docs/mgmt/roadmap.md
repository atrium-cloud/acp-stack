# Roadmap

This roadmap is a planning document for maintainers. Product contracts live under [../specs](../specs).

## Initial Release Areas

| Area             | Goal                                                                                              |
| ---------------- | ------------------------------------------------------------------------------------------------- |
| Local runtime    | one configured ACP agent, durable state, config import/export, workspace files, mediated commands |
| Trust layer      | encrypted secrets, API key tiers, permission review, rate limits, origin checks                   |
| Agent support    | small verified headless catalog with native and adapter-backed agents                             |
| MCP              | declared stdio/HTTP servers attached to sessions with secret refs                                 |
| Logs and metrics | local SQLite history, live WebSocket events, optional external analytics sink                     |
| Packaging        | Docker image, systemd installer, reverse-proxy and tunnel guidance                                |
| Operations       | readiness/status checks, dependency checks, security self-check, keyless local `acps` views       |
| Client polish    | SDKs, stronger session UX, richer log filters, operational health summaries                       |

## Later Scope

The following remain outside the initial release:

- multiple active agents per runtime
- broad cross-distro package/runtime reconciliation
- complete OS-level interception of arbitrary shell activity
- built-in TLS termination or advanced WAF policy
- snapshots and hibernation
- hosted fleet management
- billing and tenant management

## Success Criteria

The initial release is successful when an operator can:

1. Install `acp-stack` on a Linux instance.
2. Run `acps init`.
3. Configure one supported headless ACP agent (an agent accepting non-interactive API-key auth).
4. Store secrets without writing plaintext values to config.
5. Export and import reusable TOML config.
6. Validate dependency and MCP declarations.
7. Start the daemon.
8. Create or resume an agent session through CLI or HTTP.
9. Send a prompt and stream updates over WebSocket.
10. Browse, upload, download, read, and write workspace files.
11. Run mediated shell commands.
12. Receive and answer permission requests.
13. Query sessions, events, commands, and permission decisions from durable logs.
14. Deploy behind Docker or systemd with a supported public edge.
