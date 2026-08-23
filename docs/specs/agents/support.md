# Agent Support

`acp-stack` supports ACP agents only when they can run headlessly inside a self-hosted Linux instance.

## Eligibility

An agent can be supported when it:

- Supports non-interactive authentication via API key env var or configs
- Supports ACP communication natively or via adapter
- Can be installed, interacted with, and updated via command line
- Is intended for general-purpose use

## Supported Agents

The supported agents, their native-or-adapter path, adapter names, and Agent Skills support live in [data/agents.toml](../../../data/agents.toml), which is embedded into the runtime at build time.

MCP support is determined per install from the agent's ACP `initialize` advertisement (see [registry.md](registry.md)); the runtime honors only servers the advertisement covers.

Per-agent setup notes live under [../../agents](../../agents).

Agent Skills install locations, link-directory mirroring, and the embedded skills catalog are covered in [skills.md](skills.md).

## Currently Unsupported

- Cortex Code: Snowflake-specific, not a general-purpose ACP target.
- Cline: in ACP mode its only credential is a Cline account key (`CLINE_API_KEY`) obtained through browser OAuth plus phone verification, with no documented headless provider-key path, so it fails the API-key eligibility bar. It can still run as a custom harness (`cline --acp`).

## Deprecated

- Cursor CLI: set up a custom harness pointing to Cursor install script, or use Cursor's first-party cloud agent products instead.
