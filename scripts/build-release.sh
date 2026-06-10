#!/usr/bin/env bash
set -euo pipefail

# Cross-compiles the release binaries for every published Linux target and
# packages them into per-target tarballs plus a SHA256SUMS manifest under
# dist/. Runs on the maintainer's machine (macOS) via cargo-zigbuild, which
# both cross-compiles to Linux and pins the glibc floor in the target triple
# so the artifacts run on old and new distros alike.
#
# Usage: scripts/build-release.sh
#
# The git tag for a release must be `v<version>` where <version> is the
# [package] version in Cargo.toml; install.sh derives the artifact filename
# from the tag by stripping the leading `v`.

# ----- CONSTANTS -----
readonly PROJECT="acp-stack"
readonly GLIBC="2.17"
readonly TARGETS=(
  "x86_64-unknown-linux-gnu"
  "aarch64-unknown-linux-gnu"
)
readonly BINARIES=(acps acpctl)
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly DIST_DIR="${REPO_ROOT}/dist"
# ---------------------

# macOS `tar` writes AppleDouble (._*) sidecar entries unless told not to;
# they would pollute the released tarball.
export COPYFILE_DISABLE=1

log() { printf 'build-release: %s\n' "$*" >&2; }
fail() { printf 'build-release: error: %s\n' "$*" >&2; exit 1; }

command -v cargo-zigbuild >/dev/null 2>&1 \
  || fail "cargo-zigbuild not found; install with: cargo install cargo-zigbuild (and a zig toolchain)"

cd "${REPO_ROOT}"

# Single source of truth for the version: the Cargo manifest. Parsed without
# sed so the script has no awk/sed dependency beyond tarball packaging.
version_line="$(grep -m1 '^version = ' Cargo.toml)" || fail "could not read version from Cargo.toml"
version="${version_line#version = \"}"
version="${version%\"}"
[[ -n "${version}" ]] || fail "parsed an empty version from Cargo.toml"
log "building ${PROJECT} v${version} for: ${TARGETS[*]}"

# cargo-zigbuild compiles with zig but still links the rustup-managed Rust std
# for each target, so the std component must be present.
command -v rustup >/dev/null 2>&1 || fail "rustup not found; needed to install the Linux std targets"
for target in "${TARGETS[@]}"; do
  rustup target add "${target}"
done

target_args=()
for target in "${TARGETS[@]}"; do
  target_args+=(--target "${target}.${GLIBC}")
done

cargo zigbuild --release "${target_args[@]}" --bin acps --bin acpctl

rm -rf "${DIST_DIR}"
mkdir -p "${DIST_DIR}"

for target in "${TARGETS[@]}"; do
  # zigbuild normalizes the output directory to the base triple, dropping the
  # `.${GLIBC}` suffix used on the command line.
  out_dir="target/${target}/release"
  stage="$(mktemp -d)"
  for binary in "${BINARIES[@]}"; do
    [[ -x "${out_dir}/${binary}" ]] || fail "expected binary missing: ${out_dir}/${binary}"
    cp "${out_dir}/${binary}" "${stage}/${binary}"
  done
  tarball="${PROJECT}-${version}-${target}.tar.gz"
  tar -czf "${DIST_DIR}/${tarball}" -C "${stage}" "${BINARIES[@]}"
  rm -rf "${stage}"
  log "packaged ${tarball}"
done

# Bundle the installer entrypoint so it can be uploaded as a release asset
# alongside the tarballs (curl|bash users can also fetch it from the repo).
cp "${REPO_ROOT}/install.sh" "${DIST_DIR}/install.sh"

(
  cd "${DIST_DIR}"
  shasum -a 256 "${PROJECT}-${version}-"*.tar.gz > SHA256SUMS
)
log "wrote ${DIST_DIR}/SHA256SUMS"

cat >&2 <<EOF

build-release: done. Artifacts in ${DIST_DIR}:
$(cd "${DIST_DIR}" && ls -1)

To publish (after committing + tagging v${version}):
  gh release create v${version} \\
    dist/${PROJECT}-${version}-*.tar.gz \\
    dist/SHA256SUMS dist/install.sh \\
    --title "v${version}" --notes "..."
EOF
