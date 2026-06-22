#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

profile="base"
browser_use_prefix="/opt/acp-stack/browser-use"
browser_use_share="/usr/local/share/acp-stack"
browser_use_launcher="/usr/local/bin/acp-stack-browser-use-mcp"
browser_use_wrapper="${browser_use_share}/browser-use-mcp.py"
browser_use_python_version="3.14"

# Central package manifests for the Debian/Ubuntu VM image profile. The base
# set intentionally excludes build toolchains; agent-specific installers and
# explicit dependency declarations remain responsible for anything heavier.
readonly BASE_APT_PACKAGES=(
  ca-certificates
  bash
  curl
  git
  openssh-client
  nodejs
  npm
  python3
  python3-venv
  tar
  gzip
  xz-utils
  zstd
  unzip
  zip
  jq
  ripgrep
  patch
  diffutils
  procps
)

readonly BUILD_HEAVY_APT_PACKAGES=(
  build-essential
  pkg-config
  python3-dev
)

readonly BROWSER_FONT_APT_PACKAGES=(
  fonts-noto
  fonts-noto-color-emoji
  fonts-noto-cjk
  fonts-liberation
  fonts-dejavu
  fonts-freefont-ttf
)

usage() {
  cat <<'USAGE'
Usage: sudo bash scripts/install-agent-vm-deps.sh [options]

Installs the OS/userland dependencies expected by acp-stack Linux VM images.

Options:
  --profile <base|browser>  Dependency profile to install (default: base)
  --browser-use-prefix DIR  Managed Browser Use virtualenv path
                            (default: /opt/acp-stack/browser-use)
  -h, --help                Show this message

Profiles:
  base     Common agent work tools only.
  browser  Base tools plus Chromium, fonts, browser-use[core], and the
           acp-stack Browser Use MCP launcher.
USAGE
}

log() {
  printf 'install-agent-vm-deps: %s\n' "$*" >&2
}

fail() {
  printf 'install-agent-vm-deps: error: %s\n' "$*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      profile="$2"
      shift 2
      ;;
    --browser-use-prefix)
      [[ $# -ge 2 ]] || { usage >&2; exit 1; }
      browser_use_prefix="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'install-agent-vm-deps: unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

case "${profile}" in
  base|browser) ;;
  *) fail "unsupported profile: ${profile}" ;;
esac

if [[ "${EUID}" -ne 0 ]]; then
  fail "must run as root"
fi

if [[ "${browser_use_prefix}" != /* ]]; then
  fail "--browser-use-prefix must be an absolute path"
fi

is_debian_like=false
if [[ -r /etc/os-release ]]; then
  # shellcheck disable=SC1091
  . /etc/os-release
  case " ${ID:-} ${ID_LIKE:-} " in
    *' debian '*|*' ubuntu '*) is_debian_like=true ;;
  esac
fi

if [[ "${is_debian_like}" != true ]]; then
  fail "the VM dependency profile currently supports Debian/Ubuntu images"
fi

assert_base_excludes_build_packages() {
  local base_package
  local heavy_package
  for base_package in "${BASE_APT_PACKAGES[@]}"; do
    for heavy_package in "${BUILD_HEAVY_APT_PACKAGES[@]}"; do
      if [[ "${base_package}" == "${heavy_package}" ]]; then
        fail "base profile must not include build-heavy package ${base_package}"
      fi
    done
  done
}

install_apt_packages() {
  local packages=("$@")
  log "installing apt packages: ${packages[*]}"
  DEBIAN_FRONTEND=noninteractive apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "${packages[@]}"
}

install_uv() {
  if command -v uv >/dev/null 2>&1; then
    log "uv already installed; skipping."
    return
  fi

  log "installing uv with the Astral standalone installer."
  local tmp_installer
  tmp_installer="$(mktemp)"
  curl -LsSf https://astral.sh/uv/install.sh -o "${tmp_installer}"
  UV_INSTALL_DIR=/usr/local/bin UV_NO_MODIFY_PATH=1 sh "${tmp_installer}"
  rm -f "${tmp_installer}"
  command -v uv >/dev/null 2>&1 || fail "uv installer completed but uv is not on PATH"
}

install_browser_profile() {
  local chromium_package
  local chromium_cmd
  chromium_package="$(resolve_chromium_package)"
  install_apt_packages "${chromium_package}" "${BROWSER_FONT_APT_PACKAGES[@]}"

  log "creating Browser Use Python ${browser_use_python_version} virtualenv at ${browser_use_prefix}."
  install -d -m 0755 "$(dirname "${browser_use_prefix}")"
  uv venv --python "${browser_use_python_version}" "${browser_use_prefix}"
  verify_browser_python "${browser_use_prefix}/bin/python"
  uv pip install --python "${browser_use_prefix}/bin/python" --upgrade 'browser-use[core]'
  "${browser_use_prefix}/bin/browser-use" install

  log "installing Browser Use MCP launcher."
  install -d -m 0755 "${browser_use_share}"
  install -m 0644 "${REPO_ROOT}/scripts/browser-use-mcp.py" "${browser_use_wrapper}"
  render_browser_launcher "${browser_use_launcher}" "${browser_use_prefix}" "${browser_use_wrapper}"

  chromium_cmd="$(resolve_chromium_command)"
  "${chromium_cmd}" --headless --disable-gpu --no-sandbox --dump-dom about:blank >/dev/null
  "${browser_use_launcher}" --help >/dev/null
}

resolve_chromium_package() {
  if apt-cache show chromium >/dev/null 2>&1; then
    printf 'chromium\n'
    return
  fi
  if apt-cache show chromium-browser >/dev/null 2>&1; then
    printf 'chromium-browser\n'
    return
  fi
  fail "no Chromium package found in apt metadata"
}

resolve_chromium_command() {
  if command -v chromium >/dev/null 2>&1; then
    command -v chromium
    return
  fi
  if command -v chromium-browser >/dev/null 2>&1; then
    command -v chromium-browser
    return
  fi
  fail "Chromium package installed but no chromium command is on PATH"
}

render_browser_launcher() {
  local destination="$1"
  local prefix="$2"
  local wrapper="$3"
  local tmp_launcher
  tmp_launcher="$(mktemp)"
  awk \
    -v venv="${prefix}" \
    -v script="${wrapper}" '
    {
      gsub(/@BROWSER_USE_VENV@/, venv)
      gsub(/@BROWSER_USE_MCP_SCRIPT@/, script)
      print
    }
  ' "${REPO_ROOT}/scripts/browser-use-mcp" >"${tmp_launcher}"
  install -m 0755 "${tmp_launcher}" "${destination}"
  rm -f "${tmp_launcher}"
}

verify_browser_python() {
  local python="$1"
  "${python}" - <<'PY'
import sys

if sys.version_info < (3, 11):
    raise SystemExit(f"Browser Use requires Python 3.11+; venv has {sys.version.split()[0]}")
PY
}

assert_base_excludes_build_packages
install_apt_packages "${BASE_APT_PACKAGES[@]}"
install_uv

if [[ "${profile}" == "browser" ]]; then
  install_browser_profile
fi

log "profile ${profile} installed."
