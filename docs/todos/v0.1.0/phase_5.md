# Phase 5 Todo - Client And Operations Polish

All Phase 5 work is complete except the open items below. See [docs/mgmt/roadmap.md](../../mgmt/roadmap.md) for phase context.

## Session Progress And Reconnect

- [x] Replace the local fork message-id extension with ACP session cursors (`cursorId` on `session/fork`).
- [ ] Retire the per-adapter `jetbrains-air` fallback once session cursors cover the prompts it still serves: a forked child's inherited prompts and prompts the adapter announces no cursor for. Specified in [docs/specs/acp/acp-bridge.md](../../specs/acp/acp-bridge.md), heading "Fork Points".

## Initial Release Acceptance

- [ ] A user can install `acp-stack` on a Linux instance.
