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
  if [ "$(id -u)" = "0" ]; then
    export ACP_STACK_ALLOW_ROOT="${ACP_STACK_ALLOW_ROOT:-1}"
  fi
fi

if [ "${ACP_STACK_AUTO_INIT:-0}" = "1" ] && [ ! -f "${config_path}" ]; then
  echo "acp-stack: config missing; running acps init --no-install-agent" >&2
  mkdir -p /workspace /workspace/uploads \
    "${home}/.config/acp-stack" \
    "${home}/.local/share/acp-stack"
  acps init --no-install-agent
fi

exec "$@"
