#!/usr/bin/env bash
set -euo pipefail

readonly DEFAULT_BASE_URL="http://127.0.0.1:7700"
readonly DEFAULT_PROMPT="Reply with the single word: ready"
readonly DEFAULT_POLL_INTERVAL_SECONDS=5
readonly DEFAULT_POLL_COUNT=10

readonly BASE_URL="${ACP_STACK_LATENCY_PROBE_URL:-$DEFAULT_BASE_URL}"
readonly SESSION_KEY="${ACP_STACK_SESSION_KEY:-}"
readonly SESSION_CWD="${ACP_STACK_LATENCY_PROBE_CWD:-}"
readonly PROMPT_TEXT="${ACP_STACK_LATENCY_PROBE_PROMPT:-$DEFAULT_PROMPT}"
readonly POLL_INTERVAL_SECONDS="${ACP_STACK_LATENCY_PROBE_POLL_INTERVAL:-$DEFAULT_POLL_INTERVAL_SECONDS}"
readonly POLL_COUNT="${ACP_STACK_LATENCY_PROBE_POLL_COUNT:-$DEFAULT_POLL_COUNT}"

usage() {
  cat <<'USAGE'
Usage: scripts/latency-probe.sh

Against an already-running acps, creates a session, submits one prompt, polls
its status, then lists sessions twice. Prints client-side latency per request
and a per-label min/max/count summary.

Environment:
  ACP_STACK_SESSION_KEY                  session key (required)
  ACP_STACK_LATENCY_PROBE_URL            base URL, default http://127.0.0.1:7700
  ACP_STACK_LATENCY_PROBE_CWD            session cwd, optional
  ACP_STACK_LATENCY_PROBE_PROMPT         prompt text
  ACP_STACK_LATENCY_PROBE_POLL_INTERVAL  seconds between polls, default 5
  ACP_STACK_LATENCY_PROBE_POLL_COUNT     number of polls, default 10
USAGE
}

for arg in "$@"; do
  case "$arg" in
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

for tool in curl jq; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool is required" >&2
    exit 1
  fi
done

if [[ -z "$SESSION_KEY" ]]; then
  echo "error: set ACP_STACK_SESSION_KEY to an acps session key" >&2
  exit 1
fi

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

body_file="$tmpdir/body.json"
# One "label ms" line per request; bash 3.2 (macOS) has no associative arrays.
samples_file="$tmpdir/samples.txt"
last_code=""

# Sets last_code and leaves the output line open for the caller's detail column.
request() {
  local label="$1" method="$2" path="$3" body="${4:-}"
  local curl_args=(-sS -o "$body_file" -w '%{http_code} %{time_total}' -X "$method" -H "Authorization: Bearer ${SESSION_KEY}")
  if [[ -n "$body" ]]; then
    curl_args+=(-H 'Content-Type: application/json' --data "$body")
  fi
  local metrics milliseconds
  : > "$body_file"
  # curl still prints `000 <time>` when it cannot connect; that case is reported below.
  metrics="$(curl "${curl_args[@]}" "${BASE_URL}${path}" || true)"
  last_code="${metrics%% *}"
  if [[ "$last_code" == "000" ]]; then
    echo "error: $method $path got no response from $BASE_URL" >&2
    exit 1
  fi
  milliseconds="$(awk -v seconds="${metrics##* }" 'BEGIN { printf "%.1f", seconds * 1000 }')"
  echo "$label $milliseconds" >> "$samples_file"
  printf '%-14s %4s %9s ms  ' "$label" "$last_code" "$milliseconds"
}

require_success() {
  if [[ "$last_code" != 2?? ]]; then
    echo
    echo "error: $1 returned HTTP $last_code: $(<"$body_file")" >&2
    exit 1
  fi
}

printf '%-14s %4s %12s  %s\n' "request" "http" "latency" "detail"

if [[ -n "$SESSION_CWD" ]]; then
  create_body="$(jq -nc --arg cwd "$SESSION_CWD" '{cwd: $cwd}')"
else
  create_body='{}'
fi
request create_session POST /v1/sessions "$create_body"
require_success "POST /v1/sessions"
session_id="$(jq -r '.data.id' "$body_file")"
echo "session=$session_id"

request submit_prompt POST "/v1/sessions/${session_id}/prompt" "$(jq -nc --arg prompt "$PROMPT_TEXT" '{prompt: $prompt}')"
require_success "POST /v1/sessions/${session_id}/prompt"
prompt_id="$(jq -r '.data.prompt_id' "$body_file")"
echo "prompt=$prompt_id"

for ((poll = 1; poll <= POLL_COUNT; poll++)); do
  sleep "$POLL_INTERVAL_SECONDS"
  request poll_prompt GET "/v1/sessions/${session_id}/prompts/${prompt_id}"
  echo "status=$(jq -r '.data.status // .error.code' "$body_file")"
done

for _ in 1 2; do
  request list_sessions GET /v1/sessions
  echo "agent_sync=$(jq -r '.data.agent_sync.status // .error.code' "$body_file")"
done

echo
awk '
  !($1 in count) { order[++labels] = $1; min[$1] = $2 + 0; max[$1] = $2 + 0 }
  {
    count[$1]++
    if ($2 + 0 < min[$1]) min[$1] = $2 + 0
    if ($2 + 0 > max[$1]) max[$1] = $2 + 0
  }
  END {
    printf "%-14s %9s %9s %5s\n", "summary", "min_ms", "max_ms", "count"
    for (position = 1; position <= labels; position++) {
      label = order[position]
      printf "%-14s %9.1f %9.1f %5d\n", label, min[label], max[label], count[label]
    }
  }
' "$samples_file"
