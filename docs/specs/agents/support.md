# Agent Support

`acp-stack` supports ACP agents only when they can run headlessly inside a self-hosted Linux instance.

## Eligibility

An agent can be supported when it:

- Supports non-interactive authentication via API key env var or configs
- Supports ACP communication natively or via adapter
- Can be installed, interacted with, and updated via command line
- Is intended for general-purpose use

Agents that require browser OAuth, account cookies, or TUI-only setup are not supported.

## Supported Agents

| Agent      | Path    | Adapter     |
| ---------- | ------- | ----------- |
| OpenCode   | native  |             |
| Pi Agent   | adapter | `pi-acp`    |
| Amp Code   | adapter | `amp-acp`   |
| Cursor CLI | native  |             |
| Goose      | native  |             |
| Codex CLI  | adapter | `codex-acp` |

Per-agent setup notes live under [../../agents](../../agents).

## Currently Unsupported

| Agent       | Reason                                                                     |
| ----------- | -------------------------------------------------------------------------- |
| Cortex Code | Snowflake-specific, not a general-purpose ACP target                       |
| Kilo        | exposes a host/port ACP server rather than a stdio ACP peer                |
| Cline       | ACP session setup requires an auth path that is not headless API-key based |
