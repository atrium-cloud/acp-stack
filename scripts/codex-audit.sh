#!/bin/bash
# Codex audit — runs OpenAI Codex CLI in headless mode to audit staged diffs.
# Outputs only the final audit summary to stdout.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "Error: not inside a git repository." >&2
    exit 1
}
cd "$REPO_ROOT"

if ! command -v codex &>/dev/null; then
    echo "Error: codex CLI not found on PATH. Install it first." >&2
    exit 1
fi

# Stage all current changes
git add -A

# Verify there's something staged
if git diff --cached --quiet; then
    echo "Nothing staged to audit."
    exit 0
fi

PROMPT="Audit staged diffs for issues. Examine underlying logic for correctness. Classify found issues by priority (P0, P1, P2). Make no changes."

OUTFILE=$(mktemp "$TMPDIR/codex-audit.XXXXXX")
trap 'rm -f "$OUTFILE"' EXIT

echo "Running Codex audit..." >&2

codex exec --sandbox read-only --ephemeral -o "$OUTFILE" "$PROMPT" >/dev/null 2>&1

cat "$OUTFILE"
