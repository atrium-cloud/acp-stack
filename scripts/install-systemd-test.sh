#!/usr/bin/env bash
set -euo pipefail

# Runs the systemd installer end-to-end inside a privileged Docker container
# that boots systemd as PID 1. The host must be Linux with cgroup v2 (GitHub
# Actions ubuntu-latest qualifies). Docker Desktop on macOS is not supported:
# its Linux VM does not consistently expose cgroupns=host the way the
# systemd-as-PID-1 pattern needs.

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly SYSTEMD_TEST_IMAGE_DOCKERFILE="${REPO_ROOT}/packaging/systemd/installer-test.Dockerfile"
readonly CONTAINER_NAME="acp-stack-systemd-test-$$"
readonly DEFAULT_BASE_IMAGE="ubuntu:24.04"
readonly DEFAULT_IMAGE="acp-stack-systemd-test:ubuntu-24.04"
readonly BASE_IMAGE="${ACP_STACK_SYSTEMD_TEST_BASE_IMAGE:-${DEFAULT_BASE_IMAGE}}"
readonly IMAGE="${ACP_STACK_SYSTEMD_TEST_IMAGE:-${DEFAULT_IMAGE}}"
readonly HOST_PORT="${ACP_STACK_SYSTEMD_TEST_PORT:-17700}"
readonly STATUS_URL="http://127.0.0.1:${HOST_PORT}/v1/status"
readonly RUNTIME_USER="${ACP_STACK_SYSTEMD_TEST_USER:-acp_test}"
readonly HOME_DIR="/home/${RUNTIME_USER}"
readonly WORKSPACE_ROOT="${ACP_STACK_SYSTEMD_TEST_WORKSPACE:-/srv/acp-stack}"
readonly INIT_AGENT="${ACP_STACK_SYSTEMD_TEST_AGENT:-}"
readonly CONFIG_PATH="${HOME_DIR}/.config/acp-stack/acp-stack.toml"
readonly UNIT_PATH="/etc/systemd/system/acp-stack.service"

stdout_capture=""

usage() {
  cat <<'USAGE'
Usage: scripts/install-systemd-test.sh

Builds the acps + acpctl binaries (or reuses an existing release build when
ACP_STACK_SKIP_BUILD=1), builds the default systemd test image when no custom
image is supplied, starts a privileged container running /sbin/init, runs
scripts/install-systemd.sh inside it, enables the unit, waits for it to become
active, and probes GET /v1/status using the session key parsed from acps init
output.

Env knobs:
  ACP_STACK_SKIP_BUILD=1         Skip cargo build (binaries must already exist
                                 at target/release/acps and acpctl)
  ACP_STACK_SYSTEMD_TEST_PORT   Host port mapped to container 7700 (default 17700)
  ACP_STACK_SYSTEMD_TEST_BASE_IMAGE
                                 Base OS for the repo-built systemd test image
                                 (default ubuntu:24.04)
  ACP_STACK_SYSTEMD_TEST_IMAGE  Prebuilt systemd-capable image to use instead
                                 of building acp-stack-systemd-test:ubuntu-24.04
  ACP_STACK_SYSTEMD_TEST_AGENT  Real supported agent id to initialize
USAGE
}

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  '') ;;
  *) usage >&2; exit 1 ;;
esac

cleanup() {
  if [[ -n "${stdout_capture}" ]]; then
    rm -f "${stdout_capture}"
  fi
  docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
}

trap cleanup EXIT

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "install-systemd-test: refusing to run on non-Linux host (got $(uname -s))." >&2
  echo "  Booting systemd as PID 1 inside Docker Desktop on macOS is not reliable." >&2
  echo "  Run this on a Linux host (e.g. a Linux VM or a GitHub Actions ubuntu-latest runner)." >&2
  exit 1
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "install-systemd-test: docker is required." >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "install-systemd-test: curl is required." >&2
  exit 1
fi

acps_binary="${REPO_ROOT}/target/release/acps"
acpctl_binary="${REPO_ROOT}/target/release/acpctl"

if [[ "${ACP_STACK_SKIP_BUILD:-0}" != "1" ]]; then
  echo "install-systemd-test: cargo build --release --bin acps --bin acpctl..."
  (cd "${REPO_ROOT}" && cargo build --release --bin acps --bin acpctl)
fi

if [[ ! -x "${acps_binary}" || ! -x "${acpctl_binary}" ]]; then
  echo "install-systemd-test: built binaries missing at ${acps_binary} or ${acpctl_binary}." >&2
  exit 1
fi

if [[ -z "${ACP_STACK_SYSTEMD_TEST_IMAGE:-}" ]]; then
  echo "install-systemd-test: building systemd test image ${IMAGE} from ${BASE_IMAGE}..."
  docker build \
    --file "${SYSTEMD_TEST_IMAGE_DOCKERFILE}" \
    --build-arg "ACP_STACK_SYSTEMD_TEST_BASE_IMAGE=${BASE_IMAGE}" \
    --tag "${IMAGE}" \
    "${REPO_ROOT}"
fi

echo "install-systemd-test: starting systemd container ${CONTAINER_NAME}..."
docker run -d \
  --name "${CONTAINER_NAME}" \
  --privileged \
  --cgroupns=host \
  --tmpfs /tmp --tmpfs /run --tmpfs /run/lock \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  -v "${REPO_ROOT}:/src:ro" \
  -p "${HOST_PORT}:7700" \
  "${IMAGE}" \
  /sbin/init >/dev/null

echo "install-systemd-test: waiting for systemd to settle..."
for _ in $(seq 1 30); do
  if docker exec "${CONTAINER_NAME}" systemctl is-system-running --wait >/dev/null 2>&1; then
    break
  fi
  state="$(docker exec "${CONTAINER_NAME}" systemctl is-system-running 2>/dev/null || true)"
  case "${state}" in
    running|degraded) break ;;
  esac
  sleep 1
done

echo "install-systemd-test: running install-systemd.sh inside container..."
if [[ -z "${INIT_AGENT}" ]]; then
  echo "install-systemd-test: ACP_STACK_SYSTEMD_TEST_AGENT is required; choose a real supported agent id." >&2
  exit 1
fi
stdout_capture="$(mktemp)"
docker exec "${CONTAINER_NAME}" \
  bash /src/scripts/install-systemd.sh \
    --acps-binary /src/target/release/acps \
    --acpctl-binary /src/target/release/acpctl \
    --user "${RUNTIME_USER}" \
    --home "${HOME_DIR}" \
    --workspace "${WORKSPACE_ROOT}" \
    --bind 0.0.0.0:7700 \
    --agent "${INIT_AGENT}" \
    --no-os-deps \
  >"${stdout_capture}" 2>&1
cat "${stdout_capture}"

session_key="$(awk '/^session key \([^)]*\): / { print $NF; exit }' "${stdout_capture}")"
if [[ -z "${session_key}" ]]; then
  echo "install-systemd-test: failed to parse session key from installer output." >&2
  exit 1
fi

echo "install-systemd-test: asserting config and unit reflect installer options..."
docker exec "${CONTAINER_NAME}" grep -Fq "root = \"${WORKSPACE_ROOT}\"" "${CONFIG_PATH}"
docker exec "${CONTAINER_NAME}" grep -Fq "uploads = \"${WORKSPACE_ROOT}/uploads\"" "${CONFIG_PATH}"
docker exec "${CONTAINER_NAME}" grep -Fq "runtime_user = \"${RUNTIME_USER}\"" "${CONFIG_PATH}"
docker exec "${CONTAINER_NAME}" grep -Fq "User=${RUNTIME_USER}" "${UNIT_PATH}"
docker exec "${CONTAINER_NAME}" grep -Fq "Group=${RUNTIME_USER}" "${UNIT_PATH}"
docker exec "${CONTAINER_NAME}" grep -Fq "WorkingDirectory=${WORKSPACE_ROOT}" "${UNIT_PATH}"
docker exec "${CONTAINER_NAME}" grep -Fq "ReadWritePaths=${WORKSPACE_ROOT} ${HOME_DIR}" "${UNIT_PATH}"

echo "install-systemd-test: verifying idempotent installer re-run..."
docker exec "${CONTAINER_NAME}" \
  bash /src/scripts/install-systemd.sh \
    --acps-binary /src/target/release/acps \
    --acpctl-binary /src/target/release/acpctl \
    --user "${RUNTIME_USER}" \
    --home "${HOME_DIR}" \
    --workspace "${WORKSPACE_ROOT}" \
    --bind 0.0.0.0:7700 \
    --agent "${INIT_AGENT}" \
    --no-os-deps \
  >/dev/null

echo "install-systemd-test: verifying --force cannot drift config-managed paths..."
if docker exec "${CONTAINER_NAME}" \
  bash /src/scripts/install-systemd.sh \
    --acps-binary /src/target/release/acps \
    --acpctl-binary /src/target/release/acpctl \
    --user "${RUNTIME_USER}" \
    --home "${HOME_DIR}" \
    --workspace "${WORKSPACE_ROOT}-drift" \
    --bind 0.0.0.0:7700 \
    --agent "${INIT_AGENT}" \
    --no-os-deps \
    --force; then
  echo "install-systemd-test: installer unexpectedly allowed --force to drift workspace config." >&2
  exit 1
fi

echo "install-systemd-test: verifying unit drift requires --force..."
docker exec "${CONTAINER_NAME}" bash -c "printf '\n# drift\n' >> ${UNIT_PATH}"
if docker exec "${CONTAINER_NAME}" \
  bash /src/scripts/install-systemd.sh \
    --acps-binary /src/target/release/acps \
    --acpctl-binary /src/target/release/acpctl \
    --user "${RUNTIME_USER}" \
    --home "${HOME_DIR}" \
    --workspace "${WORKSPACE_ROOT}" \
    --bind 0.0.0.0:7700 \
    --agent "${INIT_AGENT}" \
    --no-os-deps; then
  echo "install-systemd-test: installer unexpectedly accepted a drifted unit without --force." >&2
  exit 1
fi
docker exec "${CONTAINER_NAME}" \
  bash /src/scripts/install-systemd.sh \
    --acps-binary /src/target/release/acps \
    --acpctl-binary /src/target/release/acpctl \
    --user "${RUNTIME_USER}" \
    --home "${HOME_DIR}" \
    --workspace "${WORKSPACE_ROOT}" \
    --bind 0.0.0.0:7700 \
    --agent "${INIT_AGENT}" \
    --no-init \
    --no-os-deps \
    --force \
  >/dev/null

echo "install-systemd-test: enabling and starting acp-stack.service..."
docker exec "${CONTAINER_NAME}" systemctl enable --now acp-stack.service >/dev/null

echo "install-systemd-test: waiting for acp-stack to become active..."
active=false
for _ in $(seq 1 30); do
  state="$(docker exec "${CONTAINER_NAME}" systemctl is-active acp-stack.service 2>/dev/null || true)"
  if [[ "${state}" == "active" ]]; then
    active=true
    break
  fi
  sleep 1
done

if [[ "${active}" != true ]]; then
  echo "install-systemd-test: acp-stack failed to become active." >&2
  docker exec "${CONTAINER_NAME}" systemctl status acp-stack.service --no-pager >&2 || true
  docker exec "${CONTAINER_NAME}" journalctl -u acp-stack.service --no-pager -n 200 >&2 || true
  exit 1
fi

echo "install-systemd-test: probing ${STATUS_URL}..."
status_body="$(mktemp)"
trap 'rm -f "${status_body}"; cleanup' EXIT
status_ok=false
for _ in $(seq 1 30); do
  if curl -fsS --max-time 2 -o "${status_body}" \
       -H "Authorization: Bearer ${session_key}" \
       "${STATUS_URL}"; then
    status_ok=true
    break
  fi
  sleep 1
done

if [[ "${status_ok}" != true ]] || ! grep -Eq '"ok"[[:space:]]*:[[:space:]]*true' "${status_body}"; then
  echo "install-systemd-test: /v1/status check failed." >&2
  docker exec "${CONTAINER_NAME}" systemctl status acp-stack.service --no-pager >&2 || true
  docker exec "${CONTAINER_NAME}" journalctl -u acp-stack.service --no-pager -n 200 >&2 || true
  exit 1
fi

echo "install-systemd-test: PASS"
