#!/usr/bin/env sh
set -eu

home="${HOME:-/home/acp}"
config_path="${home}/.config/acp-stack/acp-stack.toml"

if [ "${ACP_STACK_AUTO_INIT:-0}" = "1" ] && [ ! -f "${config_path}" ]; then
  echo "acp-stack: config missing; running acps init --no-install-agent" >&2
  mkdir -p /workspace /workspace/uploads \
    "${home}/.config/acp-stack" \
    "${home}/.local/share/acp-stack"
  acps init --no-install-agent
fi

exec "$@"
