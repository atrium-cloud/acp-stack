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
- [x] Implement secret references in config.
- [x] Implement scoped secret injection for agents, MCP servers, installers, and external logging.
- [x] Document secret-ref classification for agent runtime env, MCP stdio env, MCP HTTP headers, Git credentials, S3 credentials, and install-only tokens.
- [x] Implement `acps secrets list`.
- [x] Implement `acps secrets set <name>`.
- [x] Implement `acps secrets delete <name>`.
- [x] Implement admin-key Secrets API routes.
- [x] Ensure secret values are never returned through API, CLI logs, errors, metrics, or WebSocket events.

## MCP Declarations

- [x] Define config schema for stdio MCP servers.
- [x] Define config schema for HTTP MCP servers.
- [x] Support secret interpolation in MCP environment variables.
- [x] Support secret interpolation in HTTP MCP headers.
- [x] Launch stdio MCP servers when the configured agent supports MCP.
- [x] Pass HTTP MCP server declarations to agents when supported.
- [x] Persist MCP process lifecycle events.
- [x] Document a narrow curated MCP preset set for frequently used servers.
- [x] Document that MCP presets are convenience templates, not an allowlist.
- [x] Document custom stdio MCP server declarations with command, args, and explicit env secret refs.
- [x] Document custom HTTP MCP server declarations with URL and explicit header secret refs.
- [x] Document dependency declarations that reference custom MCP server names.
- [x] Add Linear MCP example.

## Dependency Checks

- [x] Define dependency manifest shape for commands, runtimes, packages, and MCP prerequisites.
- [x] Implement dependency validation without broad automatic installation.
- [x] Implement `GET /v1/deps`.
- [x] Implement `POST /v1/deps/check`.
- [x] Implement `acps deps check`.
- [x] Report missing dependency names, expected commands, and affected runtime features.

## Permissions

- [x] Add permission request and decision tables.
- [x] Define permission request states: pending, approved, denied, expired, canceled.
- [x] Implement pending permission API.
- [x] Implement approve/deny permission API.
- [x] Implement WebSocket topic for permission updates.
- [x] Implement command policy that can allow, deny, or require approval.
- [x] Route daemon-mediated command approvals through the permission pipeline.
- [x] Route ACP `session/request_permission` through the same permission pipeline.
- [x] Resume, reject, or time out blocked ACP operations after a decision.
- [x] Persist requester, source, command/tool detail, decision, timestamps, and deciding principal.

## HTTP Security Hardening

- [x] Reject duplicate or malformed `Authorization` headers.
- [x] Add per-IP rate limiting.
- [x] Add per-key rate limiting keyed by API key hash.
- [x] Add stricter unauthenticated rate limits.
- [x] Add temporary IP blocks after repeated authentication failures.
- [x] Add CORS allowlist.
- [x] Add WebSocket origin checks.
- [x] Add structured security logs for auth failures, rate-limit hits, IP blocks, denied origins, and oversized requests.
- [x] Respect `trust_proxy_headers` only for configured trusted proxy addresses.

## Acceptance

- [x] A user can add, list, and delete secrets without plaintext storage.
- [x] Agent and MCP processes receive only explicitly referenced secrets.
- [x] Dependency status can be queried through CLI and API.
- [x] Commands can require approval before execution.
- [x] ACP permission requests appear in the same pending-permission API as command requests.
- [x] Browser-facing clients must satisfy CORS and WebSocket origin policy.
- [x] Rate limiting and auth-failure blocks produce durable security logs.
