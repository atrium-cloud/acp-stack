# Tech Stack

This document records technology choices that affect maintenance or deployment. It is not an operator guide.

## Runtime

| Technology                | Use                                                        |
| ------------------------- | ---------------------------------------------------------- |
| Rust                      | runtime, CLI, daemon, and local interface                  |
| Tokio                     | async process, IO, timers, and networking                  |
| Axum                      | HTTP routing and middleware                                |
| SQLite                    | local durable state                                        |
| age-compatible encryption | local secret store                                         |
| agent-client-protocol SDK | ACP client boundary to agents                              |
| rmcp                      | local MCP server/client integration for `acpctl mcp serve` |
| rpassword                 | hidden terminal prompts for admin-key entry                |

## Protocols And Interfaces

| Interface          | Purpose                                   |
| ------------------ | ----------------------------------------- |
| HTTP/JSON          | remote API                                |
| WebSocket          | live event subscriptions                  |
| Unix domain socket | local `acpctl` transport                  |
| ACP over stdio     | agent protocol boundary                   |
| MCP stdio/HTTP     | optional tools attached to agent sessions |

## Deployment

| Tooling           | Use                           |
| ----------------- | ----------------------------- |
| Docker            | container deployment          |
| systemd           | host service deployment       |
| Cloudflare Tunnel | preferred public-edge profile |
| Nginx/Caddy       | reverse proxy alternatives    |

## Maintainer Details

Development checks, test scripts, and CI notes live in [development.md](development.md).
