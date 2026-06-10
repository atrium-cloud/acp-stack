#!/usr/bin/env bash
set -euo pipefail

# acp-stack bootstrap installer.
#
#   curl -fsSL https://raw.githubusercontent.com/atrium-cloud/acp-stack/main/install.sh | bash
#
# Detects the host architecture, downloads the matching release tarball from
# GitHub Releases, checksum-verifies it, and installs `acps` + `acpctl`. By
# default it installs the binaries only and leaves you to run `acps init`. Pass
# --systemd to chain into the systemd installer (creates the runtime user, the
# unit file, and optionally runs init).
#
# Options:
#   --version <tag>   Install a specific release tag (default: latest)
#   --bin-dir <dir>   Install destination (default: /usr/local/bin when
#                     writable, else ~/.local/bin)
#   --systemd         After download, run the systemd installer (needs root/sudo)
#   --agent <id>      Agent id passed to the systemd installer's `acps init`
#   -h, --help        Show this message
#
# Environment:
#   ACP_STACK_VERSION   Same as --version.

# ----- CONSTANTS -----
REPO="atrium-cloud/acp-stack"
PROJECT="acp-stack"
BINARIES="acps acpctl"
GITHUB="https://github.com"
RAW="https://raw.githubusercontent.com"
BIN_DIR_ROOT="/usr/local/bin"
BIN_DIR_USER="${HOME}/.local/bin"
# ---------------------

VERSION="${ACP_STACK_VERSION:-latest}"
BIN_DIR=""
DO_SYSTEMD=false
AGENT_ID=""

log() { printf 'install: %s\n' "$*" >&2; }
fail() { printf 'install: error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
  sed -n '3,28p' "$0" 2>/dev/null || printf 'see script header for usage\n' >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || fail "--version needs a value"; VERSION="$2"; shift 2 ;;
    --bin-dir) [ $# -ge 2 ] || fail "--bin-dir needs a value"; BIN_DIR="$2"; shift 2 ;;
    --agent)   [ $# -ge 2 ] || fail "--agent needs a value"; AGENT_ID="$2"; shift 2 ;;
    --systemd) DO_SYSTEMD=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) fail "unknown argument: $1" ;;
  esac
done

download() {
  url="$1"; out="$2"
  if have curl; then
    curl -fsSL "$url" -o "$out" || fail "download failed: $url"
  elif have wget; then
    wget -q "$url" -O "$out" || fail "download failed: $url"
  else
    fail "need curl or wget on PATH"
  fi
}

sha256_of() {
  if have sha256sum; then sha256sum "$1" | awk '{print $1}'
  elif have shasum; then shasum -a 256 "$1" | awk '{print $1}'
  else fail "need sha256sum or shasum on PATH"
  fi
}

# acp-stack ships Linux binaries only.
os="$(uname -s)"
[ "$os" = "Linux" ] || fail "acp-stack ships Linux binaries only; detected '${os}'"

arch="$(uname -m)"
case "$arch" in
  x86_64|amd64)  target="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
  *) fail "unsupported architecture: ${arch} (supported: x86_64, aarch64)" ;;
esac

# Resolve a concrete tag. `latest` is resolved by following the redirect from
# the releases/latest page so both the asset URL and the raw repo paths (for
# the --systemd handoff) reference the same release.
resolve_tag() {
  if [ "$VERSION" != "latest" ]; then printf '%s' "$VERSION"; return; fi
  have curl || fail "resolving 'latest' needs curl; pass --version <tag> or install curl"
  effective="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "${GITHUB}/${REPO}/releases/latest")" \
    || fail "could not reach GitHub to resolve the latest release"
  case "$effective" in
    */releases/tag/*) printf '%s' "${effective##*/releases/tag/}" ;;
    *) fail "no published release found; pass --version <tag>" ;;
  esac
}

tag="$(resolve_tag)"
# Artifact filenames carry the bare version; the tag convention is v<version>.
version="${tag#v}"
tarball="${PROJECT}-${version}-${target}.tar.gz"
asset_base="${GITHUB}/${REPO}/releases/download/${tag}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

log "installing ${PROJECT} ${tag} (${target})"
download "${asset_base}/${tarball}" "${tmp}/${tarball}"
download "${asset_base}/SHA256SUMS" "${tmp}/SHA256SUMS"

expected="$(grep " ${tarball}\$" "${tmp}/SHA256SUMS" | awk '{print $1}')"
[ -n "$expected" ] || fail "no checksum entry for ${tarball} in SHA256SUMS"
actual="$(sha256_of "${tmp}/${tarball}")"
[ "$expected" = "$actual" ] || fail "checksum mismatch for ${tarball} (expected ${expected}, got ${actual})"
log "checksum verified"

tar -xzf "${tmp}/${tarball}" -C "${tmp}"
for binary in $BINARIES; do
  [ -f "${tmp}/${binary}" ] || fail "release tarball missing ${binary}"
done

if [ "$DO_SYSTEMD" = true ]; then
  log "fetching systemd installer at ${tag}"
  download "${RAW}/${REPO}/${tag}/scripts/install-systemd.sh" "${tmp}/install-systemd.sh"
  download "${RAW}/${REPO}/${tag}/packaging/systemd/acp-stack.service" "${tmp}/acp-stack.service"

  set -- --acps-binary "${tmp}/acps" --acpctl-binary "${tmp}/acpctl" \
         --unit-template "${tmp}/acp-stack.service"
  if [ -n "$AGENT_ID" ]; then
    set -- "$@" --agent "$AGENT_ID"
  else
    # Without an agent id the systemd installer cannot run init; defer it.
    set -- "$@" --no-init
  fi

  if [ "$(id -u)" = "0" ]; then
    bash "${tmp}/install-systemd.sh" "$@"
  elif have sudo; then
    sudo bash "${tmp}/install-systemd.sh" "$@"
  else
    fail "--systemd needs root; re-run as root or install sudo"
  fi
  exit 0
fi

# Binaries-only install.
if [ -n "$BIN_DIR" ]; then
  dir="$BIN_DIR"
elif [ "$(id -u)" = "0" ] || [ -w "$BIN_DIR_ROOT" ]; then
  dir="$BIN_DIR_ROOT"
else
  dir="$BIN_DIR_USER"
fi
mkdir -p "$dir" || fail "could not create install dir: ${dir}"

for binary in $BINARIES; do
  install -m 0755 "${tmp}/${binary}" "${dir}/${binary}" \
    || fail "could not install ${binary} to ${dir} (try --bin-dir or sudo)"
done
log "installed acps + acpctl to ${dir}"

case ":${PATH}:" in
  *":${dir}:"*) ;;
  *) log "note: ${dir} is not on PATH; add it: export PATH=\"${dir}:\$PATH\"" ;;
esac

cat >&2 <<EOF

install: done. Next:
  acps init --agent <id>     # initialize config, secrets, and the agent
  acps serve                 # start the daemon
  acps status                # check health

Run 'acps init --help' for provider, workspace, MCP, and edge options.
EOF
