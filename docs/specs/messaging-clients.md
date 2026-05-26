# Messaging Clients

Messaging clients can sit in front of `acp-stack` and translate chat platforms into sessions, prompts, file attachments, and permission decisions.

## Role

A messaging client is an external client of the HTTP/WebSocket API. It is not part of the trusted local runtime and should authenticate with the session key unless it truly needs admin operations.

## Session Mapping

Clients should map each chat thread or conversation to one `acp-stack` session. They can store their own platform metadata externally and keep only session ids inside `acp-stack`.

## Prompt Flow

Recommended flow:

1. Create or resume the mapped session.
2. Upload or write any user-provided files into the workspace.
3. Submit the prompt.
4. Subscribe to `sessions.{id}` over WebSocket or poll prompt status.
5. Render terminal success, error, or cancellation back to the messaging user.

## Permissions

When a permission request is created, the messaging client may render it to the human operator and call approve or deny endpoints. The client should clearly show the command, path, or action being requested.

## Files And Attachments

Attachments should be written under the configured uploads path. Generated files can be returned through workspace download routes.

## Identity And Authorization

Messaging clients should keep platform user identity outside `acp-stack` unless a future API explicitly accepts that metadata. Do not expose admin-key actions to ordinary chat users.
