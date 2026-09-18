# acp-stack todos

This is the authoritative delivery queue for `atrium-cloud/acp-stack`. Completion status is maintained here. External work driven by webui or Platform is included only when it gates a product surface and is labeled accordingly. Per-version planning notes live under `docs/todos/`.

acp-stack stays vendor-neutral: Atrium-specific behavior is limited to release-workflow publication, and Platform authentication, platform-side release-channel assignment, Sprite compatibility policy, and daemon-mediated update orchestration stay outside this repository.

## Sessions and the event log

- [ ] Stamp the session id on ACP-source permission decision events (`src/runtime/mediation/permissions.rs:642`). The payload already carries it as `subject_id`, but the events row's `session_id` column stays NULL because the append is unscoped, so decisions never appear in the per-session event log, webui's permission-decision fold branch is unreachable from the durable log, and decisions render only via the browser-local side channel and vanish on reload. Command-source requests have no session, so scope the stamp to ACP-source decisions. Surfaced from webui.
- [ ] Put the real cause on prompt-failure events. They carry a fixed generic `message` (`src/runtime/agent/supervisor/parse.rs:113`) while the real error text stays on the prompt row, so the transcript's error entry always reads "prompt failed"-class text with no cause and the payload's error code is never rendered. Put the cause on the event (sealed on an Enhanced Computer). Surfaced from webui.

## Release workflow

- [ ] Stop attaching binaries to GitHub Releases or retaining them as GitHub Actions artifacts; keep private tags, changelogs, and release notes if useful. On hold: the maintainer decided GitHub Release binaries stay so the public `install.sh` and self-updater keep working; revisit before enabling.

## Delivered

- [x] Record the accepted user prompt as a durable session event (2026-09-18). The supervisor writes the prompt as `session.update` rows in ACP's own `user_message_chunk` shape, one per content block, under the same state guard as the `prompts` insert and before the ACP request goes out, so a transcript replayed from the log opens on the user's turn. Rows carry source `system` rather than `acp`, which keeps them out of the stream-start probe in the multi-session status view; the chunk's `messageId` is the prompt's message id and `_meta.acpStack.promptId` names the prompt row, so a client that rendered the prompt optimistically can match its local entry against the durable one. Sealing on an Enhanced Computer needed no change here: the daemon's broker seals every event row served by the session-events and snapshot routes by row shape, not by kind. Surfaced from webui.
