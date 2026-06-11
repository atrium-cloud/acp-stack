#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly DEFAULT_UNIT_TEMPLATE="${REPO_ROOT}/packaging/systemd/acp-stack.service"
readonly DEFAULT_UPDATE_SERVICE_TEMPLATE="${REPO_ROOT}/packaging/systemd/acp-stack-update.service"
readonly DEFAULT_UPDATE_TIMER_TEMPLATE="${REPO_ROOT}/packaging/systemd/acp-stack-update.timer"

acps_binary=""
acpctl_binary=""
user_name="acp"
home_dir=""
workspace_root="/workspace"
bind_address="127.0.0.1:7700"
unit_path="/etc/systemd/system/acp-stack.service"
unit_template="${DEFAULT_UNIT_TEMPLATE}"
update_service_path="/etc/systemd/system/acp-stack-update.service"
update_timer_path="/etc/systemd/system/acp-stack-update.timer"
update_service_template="${DEFAULT_UPDATE_SERVICE_TEMPLATE}"
update_timer_template="${DEFAULT_UPDATE_TIMER_TEMPLATE}"
do_init=true
agent_id=""
install_os_deps=true
force=false
current_step="startup"

usage() {
  cat <<'USAGE'
Usage: sudo bash scripts/install-systemd.sh \
         --acps-binary <path> --acpctl-binary <path> [options]

Installs acp-stack as a systemd service on a Linux host. Idempotent: re-runs
preserve user data; pass --force to overwrite an existing unit file or re-run
acps init on an already-initialized instance.

Required:
  --acps-binary <path>     Path to a built acps binary on this host
  --acpctl-binary <path>   Path to a built acpctl binary on this host

Options:
  --user <name>            Runtime user (default: acp)
  --home <dir>             Runtime user homedir (default: /home/<user>)
  --workspace <dir>        Workspace root (default: /workspace)
  --bind <addr>            ExecStart bind address (default: 127.0.0.1:7700)
  --agent <id>             Agent id to configure during init
  --unit-path <path>       Systemd unit destination (default:
                           /etc/systemd/system/acp-stack.service)
  --unit-template <path>   Source unit file (default:
                           packaging/systemd/acp-stack.service alongside this
                           script)
  --update-service-template <path>
                           Source updater service template (default:
                           packaging/systemd/acp-stack-update.service)
  --update-timer-template <path>
                           Source updater timer template (default:
                           packaging/systemd/acp-stack-update.timer)
  --no-init                Skip acps init (two-step install: init later as the
                           runtime user)
  --no-os-deps             Skip OS dependency installation
  --force                  Overwrite an existing unit file; re-run acps init
                           if the instance is already initialized
  -h, --help               Show this message
USAGE
}

log() {
  printf 'install-systemd: %s\n' "$*" >&2
}

fail() {
  printf 'install-systemd: error in step "%s": %s\n' "${current_step}" "$*" >&2
  exit 1
}

on_error() {
  local exit_code=$?
  printf 'install-systemd: failed during step "%s" (exit %d).\n' "${current_step}" "${exit_code}" >&2
  printf 'install-systemd: no automatic rollback was performed. Existing user data was preserved.\n' >&2
  exit "${exit_code}"
}

trap on_error ERR

while [[ $# -gt 0 ]]; do
  case "$1" in
    --acps-binary)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      acps_binary="$2"
      shift 2
      ;;
    --acpctl-binary)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      acpctl_binary="$2"
      shift 2
      ;;
    --user)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      user_name="$2"
      shift 2
      ;;
    --home)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      home_dir="$2"
      shift 2
      ;;
    --workspace)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      workspace_root="$2"
      shift 2
      ;;
    --bind)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      bind_address="$2"
      shift 2
      ;;
    --agent)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      agent_id="$2"
      shift 2
      ;;
    --unit-path)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      unit_path="$2"
      shift 2
      ;;
    --unit-template)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      unit_template="$2"
      shift 2
      ;;
    --update-service-template)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      update_service_template="$2"
      shift 2
      ;;
    --update-timer-template)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      update_timer_template="$2"
      shift 2
      ;;
    --no-init)
      do_init=false
      shift
      ;;
    --no-os-deps)
      install_os_deps=false
      shift
      ;;
    --force)
      force=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'install-systemd: unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "${home_dir}" ]]; then
  home_dir="/home/${user_name}"
fi

current_step="precheck"

if [[ "${EUID}" -ne 0 ]]; then
  fail "must run as root; re-run with sudo bash scripts/install-systemd.sh ..."
fi

if [[ -z "${acps_binary}" || -z "${acpctl_binary}" ]]; then
  printf 'install-systemd: --acps-binary and --acpctl-binary are required.\n' >&2
  usage >&2
  exit 1
fi

if [[ ! -x "${acps_binary}" ]]; then
  fail "acps binary not found or not executable: ${acps_binary}"
fi
if [[ ! -x "${acpctl_binary}" ]]; then
  fail "acpctl binary not found or not executable: ${acpctl_binary}"
fi
if [[ ! -f "${unit_template}" ]]; then
  fail "unit template not found: ${unit_template}"
fi
if [[ ! -f "${update_service_template}" ]]; then
  fail "update service template not found: ${update_service_template}"
fi
if [[ ! -f "${update_timer_template}" ]]; then
  fail "update timer template not found: ${update_timer_template}"
fi

for cmd in systemctl install useradd runuser; do
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    fail "required command not found in PATH: ${cmd}"
  fi
done

distro_family=""
if [[ -r /etc/os-release ]]; then
  # shellcheck disable=SC1091
  . /etc/os-release
  case " ${ID:-} ${ID_LIKE:-} " in
    *' debian '*|*' ubuntu '*) distro_family="debian" ;;
    *' rhel '*|*' fedora '*|*' centos '*|*' rocky '*|*' almalinux '*) distro_family="rhel" ;;
    *' suse '*|*' opensuse '*|*' opensuse-leap '*|*' opensuse-tumbleweed '*) distro_family="suse" ;;
  esac
fi

current_step="os_deps"

readonly OS_DEP_PACKAGES=(ca-certificates bash curl npm)

package_installed() {
  local package="$1"
  case "${distro_family}" in
    debian) dpkg -s "${package}" >/dev/null 2>&1 ;;
    rhel|suse) rpm -q "${package}" >/dev/null 2>&1 ;;
    *)
      case "${package}" in
        ca-certificates) command -v update-ca-certificates >/dev/null 2>&1 || command -v update-ca-trust >/dev/null 2>&1 ;;
        *) command -v "${package}" >/dev/null 2>&1 ;;
      esac
      ;;
  esac
}

missing_os_dep_packages() {
  local missing=()
  local package
  for package in "${OS_DEP_PACKAGES[@]}"; do
    if ! package_installed "${package}"; then
      missing+=("${package}")
    fi
  done
  if [[ "${#missing[@]}" -gt 0 ]]; then
    printf '%s\n' "${missing[@]}"
  fi
}

install_missing_os_deps() {
  local missing=("$@")
  case "${distro_family}" in
    debian)
      log "installing OS dependencies via apt-get: ${missing[*]}"
      DEBIAN_FRONTEND=noninteractive apt-get update -qq
      DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "${missing[@]}"
      ;;
    rhel)
      log "installing OS dependencies via dnf/yum: ${missing[*]}"
      if command -v dnf >/dev/null 2>&1; then
        dnf install -y -q "${missing[@]}"
      elif command -v yum >/dev/null 2>&1; then
        yum install -y -q "${missing[@]}"
      else
        fail "missing package manager for required OS dependencies: ${missing[*]}"
      fi
      ;;
    suse)
      log "installing OS dependencies via zypper: ${missing[*]}"
      zypper --non-interactive install "${missing[@]}"
      ;;
    *)
      fail "missing required OS tools and unsupported package manager: ${missing[*]}"
      ;;
  esac
}

if [[ "${install_os_deps}" == true ]]; then
  mapfile -t missing_deps < <(missing_os_dep_packages)
  if [[ "${#missing_deps[@]}" -eq 0 ]]; then
    log "OS dependencies already installed; skipping."
  else
    install_missing_os_deps "${missing_deps[@]}"
    mapfile -t missing_deps < <(missing_os_dep_packages)
    if [[ "${#missing_deps[@]}" -ne 0 ]]; then
      fail "required OS tools still missing after install: ${missing_deps[*]}"
    fi
  fi
else
  log "skipping OS deps install (--no-os-deps)."
fi

current_step="create_user"

if id -u "${user_name}" >/dev/null 2>&1; then
  log "user '${user_name}' already exists; reusing."
else
  log "creating system user '${user_name}' with homedir ${home_dir}."
  useradd --system --shell /usr/sbin/nologin --home-dir "${home_dir}" --create-home --user-group "${user_name}"
fi

current_step="create_dirs"

install -d -o "${user_name}" -g "${user_name}" -m 0755 "${workspace_root}"
install -d -o "${user_name}" -g "${user_name}" -m 0755 "${workspace_root}/uploads"
install -d -o "${user_name}" -g "${user_name}" -m 0700 "${home_dir}/.config/acp-stack"
install -d -o "${user_name}" -g "${user_name}" -m 0700 "${home_dir}/.local/share/acp-stack"

current_step="install_binaries"

install -o root -g root -m 0755 "${acps_binary}" /usr/local/bin/acps
install -o root -g root -m 0755 "${acpctl_binary}" /usr/local/bin/acpctl
log "installed /usr/local/bin/acps and /usr/local/bin/acpctl."

current_step="acps_init"

config_file="${home_dir}/.config/acp-stack/acps-config.toml"
if [[ "${do_init}" == true ]]; then
  if [[ -f "${config_file}" && "${force}" != true ]]; then
    log "config already present at ${config_file}; skipping acps init (pass --force to re-run)."
  else
    if [[ -z "${agent_id}" ]]; then
      fail "acps init requires a real agent id; pass --agent <id> or use --no-init and run acps init later"
    fi
    log "running acps init as ${user_name} (output below contains generated API keys -- save them now)."
    printf -- '----- acps init begin -----\n' >&2
    runuser -u "${user_name}" -- env HOME="${home_dir}" /usr/local/bin/acps init \
      --non-interactive \
      --agent "${agent_id}" \
      --workspace-root "${workspace_root}" \
      --workspace-uploads "${workspace_root}/uploads" \
      --runtime-user "${user_name}"
    printf -- '----- acps init end -----\n' >&2
  fi
else
  log "skipping acps init (--no-init); run later with: sudo -u ${user_name} -H acps init --agent <id> --workspace-root ${workspace_root} --workspace-uploads ${workspace_root}/uploads --runtime-user ${user_name}"
fi

current_step="install_unit"

unit_written=false

render_and_install_unit() {
  local template="$1"
  local destination="$2"
  local label="$3"
  local tmp_unit
  tmp_unit="$(mktemp)"
  awk \
    -v user="${user_name}" \
    -v home="${home_dir}" \
    -v workspace="${workspace_root}" \
    -v bind="${bind_address}" '
    {
      gsub(/@USER@/, user)
      gsub(/@HOME_DIR@/, home)
      gsub(/@WORKSPACE_ROOT@/, workspace)
      gsub(/@BIND@/, bind)
      print
    }
  ' "${template}" >"${tmp_unit}"

  if grep -q '@[A-Z_]\+@' "${tmp_unit}"; then
    remaining="$(grep -o '@[A-Z_]\+@' "${tmp_unit}" | sort -u | tr '\n' ' ')"
    rm -f "${tmp_unit}"
    fail "${label} template still contains placeholder tokens: ${remaining}"
  fi

  if [[ -f "${destination}" ]]; then
    if cmp -s "${tmp_unit}" "${destination}"; then
      log "${label} already matches ${destination}; leaving it unchanged."
    elif [[ "${force}" == true ]]; then
      install -o root -g root -m 0644 "${tmp_unit}" "${destination}"
      unit_written=true
      log "overwrote ${label} at ${destination}."
    else
      rm -f "${tmp_unit}"
      fail "${label} already exists at ${destination} and differs from the rendered unit; pass --force to overwrite."
    fi
  else
    install -o root -g root -m 0644 "${tmp_unit}" "${destination}"
    unit_written=true
    log "installed ${label} at ${destination}."
  fi
  rm -f "${tmp_unit}"
}

render_and_install_unit "${unit_template}" "${unit_path}" "systemd unit"
render_and_install_unit "${update_service_template}" "${update_service_path}" "update service"
render_and_install_unit "${update_timer_template}" "${update_timer_path}" "update timer"

current_step="daemon_reload"

if [[ "${unit_written}" == true ]]; then
  systemctl daemon-reload
  log "ran systemctl daemon-reload."
else
  log "skipping systemctl daemon-reload; unit file was unchanged."
fi

current_step="done"

cat <<EOF

install-systemd: success.

Next steps:
  Enable + start the daemon:
    sudo systemctl enable --now acp-stack
  Enable self-update checks:
    sudo systemctl enable --now acp-stack-update.timer
  Inspect status:
    sudo systemctl status acp-stack
  Tail logs:
    sudo journalctl -u acp-stack -f

Generated API keys (if printed above between the "acps init" delimiters)
were written to the encrypted secret store at
${home_dir}/.local/share/acp-stack/secrets.age. Save the printed values now;
they are not recoverable after this terminal scrollback is lost.

EOF
