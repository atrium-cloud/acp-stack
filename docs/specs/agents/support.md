# Agents

While `acp-stack` leverages ACP to connect to agents, not all ACP-compatible agents are automatically supported by `acp-stack`.

Last updated: May 18, 2026

## Eligibility

- Authentication: an agent harness MUST support API key for authentication.
    - An agent that only supports OAuth/browser login cannot be supported.
    - `acp-stack` does not offer first-party support for OAuth or browser login.
- Headless configuration: an agent harness MUST support config-based setup.
    - An agent that requires interactive mode to set up cannot be supported.
- ACP mode: an agent needs to support `{agent-name} acp` or `{agent-name} --acp`.
- Intended for general use: an agent should be intended for public/general use.

Examples:

- OpenCode: supports [config env interpolation](https://opencode.ai/docs/config/#env-vars) for API-key auth and can set up provider through configs.
- Cursor CLI: supports `CURSOR_API_KEY` for auth and `cursor-agent acp` for native ACP mode.
- Pi Agent: supports API-key provider env refs for auth.
- Cortex Code: requires a Snowflake account and permissions; it does not appear to be a general-purpose ACP target.

Secret uptake and API-key env var defaults are defined in [api_key.md](api_key.md). Provider configuration is defined in [config.md](config.md).

## Verification

When an agent passes both stages of verification, it can be listed as supported.

- Stage 1: official documentation
    - After reviewing an agent's documentation and eligibility appears to be satisfied, it can be considered as a candidate.
    - An agent that has insufficient or incomplete documentation cannot become a candidate. It can be revisited if documentation depth and clarity improves.
- Stage 2: manual testing
    - After an agent passes stage 1, developer can start manual testing.
    - Once eligibility is tested to be true, support will be extended in `acp-stack`'s code. If specific issues are encountered in this process and cannot be overcome, the agent cannot gain support at this time. In this case, specific reason(s) that an agent remains unsupported will be updated in this documentation.

We also have limited resources for adding support. Adding each agent requires extensive human effort and time. Contributions are welcome.

## Supported

- OpenCode: native
    - Tested OpenCode Go and Cloudflare AI Gateway on May 17, 2026.
- Pi Agent: adapter-required
    - Tested OpenCode Go and Cloudflare AI Gateway on May 17, 2026.
- Amp Code: adapter-required
    - Tested with Amp Code Smart Mode on May 17, 2026. `amp-acp v0.7.0` did not advertise ACP mode config on May 19, 2026.
- Cursor CLI: native
    - Tested with active Cursor subscription on May 17, 2026.
- Goose: native

## Currently Unsupported

- Cortex Code: Snowflake-specific, not a general-purpose ACP agent
- Kilo: headless `kilo run` works with API-key provider config, but `kilo acp` starts a hostname/port-based ACP server rather than a stdio ACP peer. `acp-stack` currently launches agents over stdio, so Kilo cannot be supervised directly until Kilo exposes stdio ACP mode, an adapter exists, or `acp-stack` supports connecting to external ACP server transports (valid as of 2026-05-18).
- Cline: direct CLI prompts can use OpenRouter headlessly, and `cline --acp` initializes over newline-delimited JSON-RPC, but isolated ACP session creation is not headless-compatible. In manual testing, `session/new` failed until the client called ACP `authenticate`; Cline's advertised ACP auth methods opened browser/account login instead of using the injected OpenRouter API-key env var (valid as of 2026-05-18).
    - Cline also has an open ACP registry compatibility issue around expected session behavior in Zed: https://github.com/zed-industries/zed/issues/55913. Revisit if Cline exposes a non-interactive ACP auth path for provider API keys or supports ACP sessions from provider env/config without browser login.
