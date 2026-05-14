# Phase 2 Todo - Secrets, Permissions, And MCP

## References

- [project-spec](../specs/project-spec.md)
- [api](../specs/api/api.md)
- [acp-bridge](../specs/acp/acp-bridge.md)
- [roadmap](../mgmt/roadmap.md)

## Secrets

- [x] Add age-compatible key generation during `acps init`.
- [x] Store the age private key at `~/.config/acp-stack/age.key`.
- [x] Store encrypted secret values at `~/.local/share/acp-stack/secrets.age`.
- [ ] Implement secret references in config.
- [ ] Implement scoped secret injection for agents, MCP servers, installers, and external logging.
- [x] Implement `acps secrets list`.
- [x] Implement `acps secrets set <name>`.
- [x] Implement `acps secrets delete <name>`.
- [ ] Implement admin-key Secrets API routes.
- [ ] Ensure secret values are never returned through API, CLI logs, errors, metrics, or WebSocket events.

## MCP Declarations

- [ ] Define config schema for stdio MCP servers.
- [ ] Define config schema for HTTP MCP servers.
- [ ] Support secret interpolation in MCP environment variables.
- [ ] Support secret interpolation in HTTP MCP headers.
- [ ] Launch stdio MCP servers when the configured agent supports MCP.
- [ ] Pass HTTP MCP server declarations to agents when supported.
- [ ] Persist MCP process lifecycle events.
- [ ] Add Slack MCP example.
- [ ] Add Linear MCP example.
- [ ] Add generic HTTP MCP example.

## Dependency Checks

- [ ] Define dependency manifest shape for commands, runtimes, packages, and MCP prerequisites.
- [ ] Implement dependency validation without broad automatic installation.
- [ ] Implement `GET /v1/deps`.
- [ ] Implement `POST /v1/deps/check`.
- [ ] Implement `acps deps check`.
- [ ] Report missing dependency names, expected commands, and affected runtime features.

## Permissions

- [ ] Add permission request and decision tables.
- [ ] Define permission request states: pending, approved, denied, expired, canceled.
- [ ] Implement pending permission API.
- [ ] Implement approve/deny permission API.
- [ ] Implement WebSocket topic for permission updates.
- [ ] Implement command policy that can allow, deny, or require approval.
- [ ] Route daemon-mediated command approvals through the permission pipeline.
- [ ] Route ACP `session/request_permission` through the same permission pipeline.
- [ ] Resume, reject, or time out blocked ACP operations after a decision.
- [ ] Persist requester, source, command/tool detail, decision, timestamps, and deciding principal.

## HTTP Security Hardening

- [ ] Reject duplicate or malformed `Authorization` headers.
- [ ] Add per-IP rate limiting.
- [ ] Add per-key rate limiting keyed by API key hash.
- [ ] Add stricter unauthenticated rate limits.
- [ ] Add temporary IP blocks after repeated authentication failures.
- [ ] Add CORS allowlist.
- [ ] Add WebSocket origin checks.
- [ ] Add structured security logs for auth failures, rate-limit hits, IP blocks, denied origins, and oversized requests.
- [ ] Respect `trust_proxy_headers` only for configured trusted proxy addresses.

## Acceptance

- [x] A user can add, list, and delete secrets without plaintext storage.
- [ ] Agent and MCP processes receive only explicitly referenced secrets.
- [ ] Dependency status can be queried through CLI and API.
- [ ] Commands can require approval before execution.
- [ ] ACP permission requests appear in the same pending-permission API as command requests.
- [ ] Browser-facing clients must satisfy CORS and WebSocket origin policy.
- [ ] Rate limiting and auth-failure blocks produce durable security logs.
