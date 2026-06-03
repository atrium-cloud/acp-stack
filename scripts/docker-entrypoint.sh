#!/usr/bin/env sh
set -eu

home="${HOME:-/home/acp}"
config_path="${home}/.config/acp-stack/acp-stack.toml"
workspace_root="${ACP_STACK_INIT_WORKSPACE_ROOT:-/workspace}"
workspace_uploads="${ACP_STACK_INIT_WORKSPACE_UPLOADS:-${workspace_root}/uploads}"
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
  run_init() {
    set -- init --non-interactive --agent "${ACP_STACK_INIT_AGENT}"
    if [ -n "${ACP_STACK_INIT_PROVIDER:-}" ]; then
      set -- "$@" --provider "${ACP_STACK_INIT_PROVIDER}"
    fi
    if [ -n "${ACP_STACK_INIT_API_KEY_REF:-}" ]; then
      set -- "$@" --api-key-ref "${ACP_STACK_INIT_API_KEY_REF}"
    fi
    if [ -n "${ACP_STACK_INIT_MODEL:-}" ]; then
      set -- "$@" --model "${ACP_STACK_INIT_MODEL}"
    fi
    if [ -n "${ACP_STACK_INIT_MODE:-}" ]; then
      set -- "$@" --mode "${ACP_STACK_INIT_MODE}"
    fi
    if [ -n "${ACP_STACK_INIT_WORKSPACE_ROOT:-}" ]; then
      set -- "$@" --workspace-root "${ACP_STACK_INIT_WORKSPACE_ROOT}"
    fi
    if [ -n "${ACP_STACK_INIT_WORKSPACE_UPLOADS:-}" ]; then
      set -- "$@" --workspace-uploads "${ACP_STACK_INIT_WORKSPACE_UPLOADS}"
    fi
    acps "$@"
  }
  echo "acp-stack: config missing; running acps init" >&2
  mkdir -p "${workspace_root}" "${workspace_uploads}" \
    "${home}/.config/acp-stack" \
    "${home}/.local/share/acp-stack"
  run_init
fi

exec "$@"
