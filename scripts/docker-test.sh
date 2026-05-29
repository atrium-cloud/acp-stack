#!/usr/bin/env bash
set -euo pipefail

readonly BUILD_CONTEXT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly HOST_PORT="${ACP_STACK_DOCKER_TEST_PORT:-7700}"
readonly STATUS_URL="http://127.0.0.1:${HOST_PORT}/v1/status"
readonly INIT_AGENT="${ACP_STACK_DOCKER_TEST_AGENT:-}"

persistent=false
rebuild=false
reset=false
cleanup_volumes=false

image_tag=""
workspace_volume=""
config_volume=""
state_volume=""
container_name=""
server_container_id=""
status_file=""

usage() {
  cat <<'USAGE'
Usage: scripts/docker-test.sh [--cleanup-volumes]
       scripts/docker-test.sh --persistent [--rebuild] [--reset]

Builds the acp-stack Docker image, initializes it in a one-shot container,
starts a runtime container, checks GET /v1/status with a session key, and
cleans up the runtime container. Named volumes are preserved by default.

Persistent mode reuses:
  image:     acp-stack-test:persistent
  container: acp-stack-test-server
  volumes:   acp-stack-test-workspace/config/state

Set ACP_STACK_DOCKER_TEST_PORT to override the host port, default 7700.
Set ACP_STACK_DOCKER_TEST_AGENT to the real agent id to initialize.
USAGE
}

for arg in "$@"; do
  case "$arg" in
    --persistent)
      persistent=true
      ;;
    --rebuild)
      rebuild=true
      ;;
    --reset)
      reset=true
      ;;
    --cleanup-volumes)
      cleanup_volumes=true
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
done

if [[ "${persistent}" != true && ( "${rebuild}" == true || "${reset}" == true ) ]]; then
  echo "--rebuild and --reset require --persistent" >&2
  exit 1
fi

if [[ "${persistent}" == true && "${cleanup_volumes}" == true ]]; then
  echo "--cleanup-volumes is only valid for the default ephemeral test; use --persistent --reset for persistent state" >&2
  exit 1
fi

configure_names() {
  if [[ "${persistent}" == true ]]; then
    image_tag="acp-stack-test:persistent"
    workspace_volume="acp-stack-test-workspace"
    config_volume="acp-stack-test-config"
    state_volume="acp-stack-test-state"
    container_name="acp-stack-test-server"
  else
    local suffix="epoch-$$"
    suffix="$(date +%s)-$$"
    local name_prefix="acp-stack-test-${suffix}"
    image_tag="acp-stack-test:phase4"
    workspace_volume="${name_prefix}-workspace"
    config_volume="${name_prefix}-config"
    state_volume="${name_prefix}-state"
    container_name="${name_prefix}-server"
  fi
}

cleanup() {
  if [[ "${persistent}" != true && -n "${server_container_id}" ]]; then
    docker rm -f "${server_container_id}" >/dev/null 2>&1 || true
  fi

  if [[ -n "${status_file}" ]]; then
    rm -f "${status_file}"
  fi

  if [[ "${persistent}" != true && "${cleanup_volumes}" == true ]]; then
    docker volume rm -f "${workspace_volume}" "${config_volume}" "${state_volume}" >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT

configure_names

persistent_state_dir() {
  printf '%s\n' "${BUILD_CONTEXT}/.git/docker-test"
}

persistent_init_output_file() {
  printf '%s\n' "$(persistent_state_dir)/persistent-init-output.txt"
}

persistent_session_key_file() {
  printf '%s\n' "$(persistent_state_dir)/persistent-session-key"
}

extract_session_key() {
  awk '/^session key \([^)]*\): / { print $NF; exit }'
}

image_exists() {
  docker image inspect "${image_tag}" >/dev/null 2>&1
}

build_image_if_needed() {
  if [[ "${persistent}" == true && "${rebuild}" != true ]] && image_exists; then
    echo "Reusing image ${image_tag}."
    return
  fi
  echo "Building ${image_tag}..."
  docker build --tag "${image_tag}" "${BUILD_CONTEXT}"
}

create_volumes() {
  echo "Preparing named volumes..."
  docker volume create "${workspace_volume}" >/dev/null
  docker volume create "${config_volume}" >/dev/null
  docker volume create "${state_volume}" >/dev/null
}

remove_persistent_state() {
  echo "Resetting persistent test container and volumes..."
  docker rm -f "${container_name}" >/dev/null 2>&1 || true
  docker volume rm -f "${workspace_volume}" "${config_volume}" "${state_volume}" >/dev/null 2>&1 || true
  rm -f "$(persistent_init_output_file)" "$(persistent_session_key_file)"
}

config_volume_initialized() {
  docker run --rm \
    -v "${config_volume}:/home/acp/.config/acp-stack" \
    "${image_tag}" \
    sh -c 'test -f /home/acp/.config/acp-stack/acp-stack.toml'
}

save_persistent_init_output() {
  local init_output="$1"
  local state_dir
  state_dir="$(persistent_state_dir)"
  mkdir -p "${state_dir}"
  chmod 700 "${state_dir}"
  printf '%s\n' "${init_output}" > "$(persistent_init_output_file)"
  chmod 600 "$(persistent_init_output_file)"
  printf '%s\n' "${init_output}" | extract_session_key > "$(persistent_session_key_file)"
  chmod 600 "$(persistent_session_key_file)"
}

load_persistent_session_key() {
  local key_file
  key_file="$(persistent_session_key_file)"
  if [[ ! -s "${key_file}" ]]; then
    echo "persistent session key cache is missing; run scripts/docker-test.sh --persistent --reset" >&2
    exit 1
  fi
  local key
  key="$(<"${key_file}")"
  if [[ -z "${key}" ]]; then
    echo "persistent session key cache is empty; run scripts/docker-test.sh --persistent --reset" >&2
    exit 1
  fi
  printf '%s\n' "${key}"
}

run_init() {
  if [[ -z "${INIT_AGENT}" ]]; then
    echo "ACP_STACK_DOCKER_TEST_AGENT is required; choose a real supported agent id" >&2
    exit 1
  fi
  echo "Running one-shot init..." >&2
  local init_output
  init_output="$(
    docker run --rm \
      --name "${container_name}-init" \
      -v "${workspace_volume}:/workspace" \
      -v "${config_volume}:/home/acp/.config/acp-stack" \
      -v "${state_volume}:/home/acp/.local/share/acp-stack" \
      "${image_tag}" \
      acps init --non-interactive --agent "${INIT_AGENT}"
  )"

  local session_key
  session_key="$(printf '%s\n' "${init_output}" | extract_session_key)"
  if [[ -z "${session_key}" ]]; then
    echo "failed to parse session key from init output" >&2
    exit 1
  fi

  if [[ "${persistent}" == true ]]; then
    save_persistent_init_output "${init_output}"
    echo "Saved persistent init output to $(persistent_init_output_file)." >&2
  fi

  printf '%s\n' "${session_key}"
}

stop_existing_persistent_container() {
  if [[ "${persistent}" == true ]]; then
    docker rm -f "${container_name}" >/dev/null 2>&1 || true
  fi
}

build_image_if_needed

if [[ "${persistent}" == true && "${reset}" == true ]]; then
  remove_persistent_state
fi

create_volumes

session_key=""
if [[ "${persistent}" == true ]] && config_volume_initialized; then
  echo "Reusing initialized persistent config volume."
  session_key="$(load_persistent_session_key)"
else
  session_key="$(run_init)"
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required for the status check" >&2
  exit 1
fi

echo "Starting daemon container..."
stop_existing_persistent_container
server_container_id="$(docker run -d \
  --name "${container_name}" \
  -p "${HOST_PORT}:7700" \
  -v "${workspace_volume}:/workspace" \
  -v "${config_volume}:/home/acp/.config/acp-stack" \
  -v "${state_volume}:/home/acp/.local/share/acp-stack" \
  "${image_tag}")"

status_file="$(mktemp)"
status_exit=1
for _ in $(seq 1 30); do
  if curl -fsS --max-time 2 -o "${status_file}" \
    -H "Authorization: Bearer ${session_key}" \
    "${STATUS_URL}"; then
    status_exit=0
    break
  fi
  sleep 1
done

if [[ "${status_exit}" -ne 0 ]] || ! grep -Eq '"ok"[[:space:]]*:[[:space:]]*true' "${status_file}"; then
  echo "status check failed" >&2
  docker logs "${server_container_id}" >&2 || true
  exit 1
fi

echo "Test passed."
if [[ "${persistent}" == true ]]; then
  echo "Persistent container ${container_name} is still running."
fi
