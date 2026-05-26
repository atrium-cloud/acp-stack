# ACP Bridge

`acp-stack` is an ACP client. The configured agent is the ACP server process, launched over stdio unless an adapter provides that stdio surface.

## Initialization

When the agent starts, the bridge initializes ACP and records the advertised capabilities. Capability snapshots are exposed through the API and used to decide which session operations are available.

Initialization failure prevents the agent from becoming ready and is reported in agent status.

## Sessions

The bridge maps runtime session operations to ACP methods where supported:

- create
- list
- load
- resume
- close/delete
- prompt
- cancel
- set model or mode config options

If an agent does not advertise an optional capability, the corresponding runtime operation fails with an explicit unsupported error.

## Streaming

ACP `session/update` notifications are persisted as durable events and published to WebSocket subscribers. Prompt submission returns quickly with a prompt id; clients can follow live updates or poll durable prompt state.

## Permissions

ACP permission requests flow into the same permission system used by mediated commands. Decisions are recorded and returned to the agent through ACP.

## MCP Servers

Configured MCP servers are attached to ACP sessions when the agent and SDK support session MCP configuration. Secret refs for MCP env vars and headers are resolved at attach time and are not written to logs or API responses.
