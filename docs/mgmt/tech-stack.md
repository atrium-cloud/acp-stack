# Tech Stack

This document records technology choices that affect maintenance or deployment. It is not an operator guide.

## Runtime

| Technology                | Use                                                        |
| ------------------------- | ---------------------------------------------------------- |
| Rust                      | runtime, CLI, and daemon                                   |
| Tokio                     | async process, IO, timers, and networking                  |
| Axum                      | HTTP routing and middleware                                |
| SQLite                    | local durable state                                        |
| tokio-postgres            | Supabase Postgres logging backend                          |
| age-compatible encryption | local secret store                                         |
| agent-client-protocol SDK | ACP client boundary to agents (1.x, protocol v1 schema)    |
| cliclack                  | interactive `acps init` prompts and searchable selectors   |
| rpassword                 | hidden terminal prompts for admin-key entry                |
| clap_complete             | shell completion script generation for `acps`              |
| semver                    | release-version comparison for self-update policy          |

## Protocols And Interfaces

| Interface          | Purpose                                   |
| ------------------ | ----------------------------------------- |
| HTTP/JSON          | remote API                                |
| WebSocket          | live event subscriptions                  |
| Unix domain socket | internal local `acps` read transport      |
| ACP over stdio     | agent protocol boundary                   |
| MCP stdio/HTTP     | optional tools attached to agent sessions |

## Deployment

| Tooling           | Use                           |
| ----------------- | ----------------------------- |
| Docker            | container deployment          |
| systemd           | host service deployment       |
| uv                | optional VM Python tooling    |
| util-linux (`unshare`, `setpriv`) | agent sandbox: namespace creation and privilege drop for the `unshare` backend |
| bubblewrap (`bwrap`) | agent sandbox: unprivileged-user-namespace backend (optional) |
| Browser Use       | optional browser MCP profile  |
| Cloudflare Tunnel | preferred public-edge profile |
| Nginx/Caddy       | reverse proxy alternatives    |

## Maintainer Details

Development checks, test scripts, and CI notes live in [development.md](development.md).
