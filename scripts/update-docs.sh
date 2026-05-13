#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

llms_url="${ACP_LLMS_URL:-https://agentclientprotocol.com/llms.txt}"
docs_dir="${ACP_DOCS_DIR:-$repo_root/docs/ref/acp}"
curl_bin="${CURL_BIN:-curl}"
origin="https://agentclientprotocol.com"

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

llms_file="$tmpdir/llms.txt"
curl_args=(--ipv4 --fail --silent --show-error --location --connect-timeout 20 --retry 2)
"$curl_bin" "${curl_args[@]}" "$llms_url" > "$llms_file"

urls=()
while IFS= read -r url; do
  urls+=("$url")
done < <(
  grep -Eo 'https://agentclientprotocol\.com/[^])"[:space:]]+\.md([?#][^])"[:space:]]*)?|/[^])"[:space:]]+\.md([?#][^])"[:space:]]*)?' "$llms_file" |
    sed -E "s#^/#${origin}/#" |
    sed -E 's/[?#].*$//' |
    sort -u
)

if [ "${#urls[@]}" -eq 0 ]; then
  echo "No Markdown documentation links found in $llms_url" >&2
  exit 1
fi

staging="$tmpdir/docs"
mkdir -p "$staging"

for url in "${urls[@]}"; do
  rel="${url#"$origin"/}"
  case "$rel" in
    ""|/*|*../*|../*|*"/.."|*"//"*)
      echo "Skipping unsafe ACP docs path: $url" >&2
      continue
      ;;
  esac

  dest="$staging/$rel"
  mkdir -p "$(dirname "$dest")"
  "$curl_bin" "${curl_args[@]}" "$url" > "$dest"
  echo "Fetched $rel"
done

mkdir -p "$docs_dir"
find "$docs_dir" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
cp -R "$staging"/. "$docs_dir"/

echo "Saved ${#urls[@]} ACP Markdown document(s) to $docs_dir"
