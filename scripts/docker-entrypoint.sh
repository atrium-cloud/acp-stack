#!/usr/bin/env sh
set -eu

home="${HOME:-/home/acp}"
config_path="${home}/.config/acp-stack/acp-stack.toml"
railway_platform=0

if [ -n "${RAILWAY_PROJECT_ID:-}" ] && \
  [ -n "${RAILWAY_ENVIRONMENT_ID:-}" ] && \
  [ -n "${RAILWAY_SERVICE_ID:-}" ]; then
  railway_platform=1
fi

if [ "${railway_platform}" = "1" ]; then
  export ACP_STACK_AUTO_INIT="${ACP_STACK_AUTO_INIT:-1}"
fi

if [ "${ACP_STACK_AUTO_INIT:-0}" = "1" ] && [ ! -f "${config_path}" ]; then
  if [ -z "${ACP_STACK_INIT_AGENT:-}" ]; then
    echo "acp-stack: ACP_STACK_AUTO_INIT requires ACP_STACK_INIT_AGENT=<agent-id>" >&2
    exit 1
  fi
  echo "acp-stack: config missing; running acps init" >&2
  mkdir -p /workspace /workspace/uploads \
    "${home}/.config/acp-stack" \
    "${home}/.local/share/acp-stack"
  acps init --non-interactive --agent "${ACP_STACK_INIT_AGENT}"
fi

exec "$@"
