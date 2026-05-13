#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
script="$repo_root/scripts/update-docs.sh"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

mkdir -p "$tmpdir/source/docs/spec"
cat > "$tmpdir/source/llms.txt" <<'LLMS'
# Agent Client Protocol

- [Overview](https://agentclientprotocol.com/overview.md)
- [Protocol spec](https://agentclientprotocol.com/docs/spec/protocol.md)
LLMS

cat > "$tmpdir/source/overview.md" <<'MD'
# Overview
MD

cat > "$tmpdir/source/docs/spec/protocol.md" <<'MD'
# Protocol
MD

cat > "$tmpdir/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

url="${@: -1}"
base="${ACP_FIXTURE_BASE:?}"
case "$url" in
  https://agentclientprotocol.com/llms.txt)
    cat "$base/llms.txt"
    ;;
  https://agentclientprotocol.com/*)
    rel="${url#https://agentclientprotocol.com/}"
    cat "$base/$rel"
    ;;
  *)
    echo "unexpected URL: $url" >&2
    exit 22
    ;;
esac
SH
chmod +x "$tmpdir/curl"

PATH="$tmpdir:$PATH" \
  ACP_FIXTURE_BASE="$tmpdir/source" \
  ACP_DOCS_DIR="$tmpdir/out" \
  "$script"

test -f "$tmpdir/out/overview.md"
test -f "$tmpdir/out/docs/spec/protocol.md"
grep -q '# Overview' "$tmpdir/out/overview.md"
grep -q '# Protocol' "$tmpdir/out/docs/spec/protocol.md"

echo "update-docs fixture test passed"
