# Messaging Clients

Messaging clients let users operate `acp-stack` from chat platforms such as Slack, Discord, Telegram, WeChat, or similar tools.

This is a future-facing client integration surface. Messaging clients should not talk ACP stdio directly unless they are implementing their own runtime. For `acp-stack`, they should use the public HTTP and WebSocket API while `acp-stack` remains the runtime boundary for agents, workspace files, mediated commands, secrets, logs, and permissions.

## Role

A messaging client is an external frontend for an `acp-stack` instance:

- user messages become session prompts
- chat threads or channels map to agent sessions
- streamed session updates become message edits, replies, or thread messages
- permission requests become chat-native approval flows
- attachments become workspace uploads
- durable logs and state remain inside `acp-stack`

The messaging client owns platform-specific UX. `acp-stack` owns runtime execution and user-OS mediation.

## Boundary

Messaging integrations must preserve the same security boundary as other API clients:

- do not run shell commands directly from the bot process
- do not read or write workspace files outside the Workspace API
- do not store agent API keys in the messaging platform when secret references can be used
- do not bypass the permission pipeline for command execution or ACP permission requests
- do not treat a messaging platform admin as automatically equivalent to an `acp-stack` admin key holder

The bot should be replaceable. If a bot is removed, the instance state, sessions, logs, permissions, workspace, and secrets should remain intact in `acp-stack`.

## Session Mapping

Messaging platforms differ in how they model conversations. The integration should map platform conversation units to `acp-stack` sessions explicitly:

- direct message -> one or more user-scoped sessions
- channel thread -> one session per thread
- channel command -> create or select a session before prompting
- group chat -> shared session only when the integration has an explicit policy for participants

Session IDs should be stored by the messaging integration as platform metadata. The integration should be able to recover after restart by listing or loading sessions through the API where supported.

## Prompt Flow

Typical prompt flow:

1. User sends a message, slash command, or bot mention.
2. Messaging client resolves the target `acp-stack` session.
3. Messaging client uploads any attachments through the Workspace API.
4. Messaging client sends the prompt through the Session API.
5. Messaging client subscribes to WebSocket updates for the session.
6. Agent updates are rendered as platform messages, replies, or edits.
7. Final stop reason is reflected in the chat UI.

The messaging client should preserve enough correlation data to connect platform message IDs with `acp-stack` session, command, permission, and event IDs.

## Permissions

Permission requests should be rendered as explicit user actions in the messaging platform:

- approve once
- deny
- show request detail
- show command/tool context

High-risk requests should avoid one-tap approval if the platform UI cannot show enough context. If a platform cannot present approval context safely, the integration should link to a richer UI rather than approve inside chat.

Permission decisions must call the Permissions API. The bot should not resume blocked operations through side channels.

## Files And Attachments

Attachments should become workspace files through upload endpoints. The integration should expose resulting paths to the agent in the prompt or session context.

File reads and writes should remain mediated by `acp-stack`:

- user-provided attachments enter through Workspace API uploads
- generated artifacts can be linked back to users through download routes
- platform file IDs should be stored as metadata, not as the canonical file identity

## Identity And Authorization

The messaging client needs its own mapping from platform identities to `acp-stack` authorization policy.

Minimum policy questions:

- which platform users can create sessions
- which users can prompt existing sessions
- which users can approve permission requests
- whether channel admins imply any `acp-stack` privileges
- how shared channels, external guests, and bot mentions are handled

The `acp-stack` admin key should not be exposed to ordinary chat users. Session-key use should be scoped to the bot backend and rotated if the bot backend is compromised.

## Event Rendering

ACP session updates can include text, tool calls, plans, and progress updates. Messaging clients should render these in a compact, platform-native way:

- stream text as message edits when supported
- use replies or thread messages for tool-call progress
- collapse noisy intermediate updates
- preserve final output clearly
- surface errors with actionable context

Long outputs should be uploaded as files or linked through `acp-stack` download routes instead of flooding a channel.

## Fit With Initial Release

Messaging clients are not required for the initial `0.0.1` release. They become practical once these surfaces are stable:

- Session API
- Workspace API
- WebSocket streaming
- Permissions API
- durable logs
- deployment guides for public or private HTTP access

The initial implementation can be an example client or reference bridge after the core API is stable.
