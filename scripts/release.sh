#!/usr/bin/env bash

# Prepare an acp-stack release. Nightly releases (the default) tag the current
# main HEAD as vX.Y.Z.N without a version commit or changelog; regular and
# major releases bump the package version, pair it with a curated changelog,
# and create a release commit. The tag triggers the GitHub Actions release
# workflow, which builds and publishes the artifacts; this script only
# advances the version state and tags.

set -euo pipefail

# Release-policy constants live here so version and Git behavior are not split
# across shell call sites.
readonly RELEASE_BRANCH="main"
readonly TAG_PREFIX="v"
readonly COMMIT_PREFIX="chore: release v"
readonly SEMVER_RE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
readonly NIGHTLY_RE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
readonly RELEASE_FILES=(
    Cargo.toml
    Cargo.lock
    docs/specs/api/acps-schema.json
    docs/specs/api/acps-schema.meta.json
)

usage() {
    cat <<'EOF'
Prepare an acp-stack release tag (and, for regular/major, a release commit).

Usage:
  scripts/release.sh [--regular|--major|X.Y.Z] [--dry-run] [--push]

Release types:
  nightly (default)  Tag HEAD as vX.Y.Z.N, incrementing the nightly component
                     (v0.1.1 -> v0.1.1.1 -> v0.1.1.2). For small fixes and
                     minor incremental changes: no changelog, no version
                     commit; the workflow publishes it as a GitHub prerelease.
  --regular          Bump vX.Y.Z -> vX.Y.(Z+1). Marks a larger feature
                     addition or refactor; requires a curated changelog.
  --major            Bump vX.Y.Z -> vX.(Y+1).0. Same changelog requirement.
  X.Y.Z              Explicit version, regular semantics.

A curated changelog must already exist at docs/changelogs/vX.Y.Z.md for
regular, major, and explicit releases. Without --push, the release commit
(if any) and tag remain local.
EOF
}

fail() {
    printf 'release: error: %s\n' "$*" >&2
    exit 1
}

read_package_version() {
    awk '
        /^version[[:space:]]*=/ {
            line = $0
            sub(/^[^"]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            found++
            exit
        }
        END { if (found != 1) exit 1 }
    ' Cargo.toml
}

write_package_version() {
    local version="$1"
    local temp_file
    temp_file="$(mktemp "${TMPDIR:-/tmp}/acp-stack-release-version.XXXXXX")"
    if ! awk -v version="$version" '
        !replaced && /^version[[:space:]]*=/ {
            print "version = \"" version "\""
            replaced++
            next
        }
        { print }
        END { if (replaced != 1) exit 1 }
    ' Cargo.toml >"$temp_file"; then
        rm -f -- "$temp_file"
        fail "Cargo.toml must contain a [package] version"
    fi
    cat "$temp_file" >Cargo.toml
    rm -f -- "$temp_file"
}

version_is_greater() {
    local candidate="$1"
    local current="$2"
    local candidate_major candidate_minor candidate_patch
    local current_major current_minor current_patch
    IFS=. read -r candidate_major candidate_minor candidate_patch <<<"$candidate"
    IFS=. read -r current_major current_minor current_patch <<<"$current"
    if ((10#$candidate_major != 10#$current_major)); then
        ((10#$candidate_major > 10#$current_major))
    elif ((10#$candidate_minor != 10#$current_minor)); then
        ((10#$candidate_minor > 10#$current_minor))
    else
        ((10#$candidate_patch > 10#$current_patch))
    fi
}

next_version() {
    local current="$1"
    local bump="$2"
    local major minor patch
    IFS=. read -r major minor patch <<<"$current"
    case "$bump" in
        patch) printf '%d.%d.%d\n' "$((10#$major))" "$((10#$minor))" "$((10#$patch + 1))" ;;
        minor) printf '%d.%d.0\n' "$((10#$major))" "$((10#$minor + 1))" ;;
        major) printf '%d.0.0\n' "$((10#$major + 1))" ;;
        *)
            [[ "$bump" =~ $SEMVER_RE ]] || fail "invalid version or bump: $bump"
            version_is_greater "$bump" "$current" \
                || fail "explicit version $bump must be greater than $current"
            printf '%s\n' "$bump"
            ;;
    esac
}

# The latest release tag may be a stable vX.Y.Z tag or a nightly vX.Y.Z.N tag
# of the same base version; git's version sort orders both shapes correctly.
latest_release_tag() {
    local tag version
    while IFS= read -r tag; do
        version="${tag#v}"
        if [[ "$version" =~ $SEMVER_RE || "$version" =~ $NIGHTLY_RE ]]; then
            printf '%s\n' "$tag"
            return 0
        fi
    done < <(git tag --list 'v*' --sort=-version:refname)
    return 1
}

release_type="nightly"
explicit_version=""
do_push=0
dry_run=0
for argument in "$@"; do
    case "$argument" in
        --push) do_push=1 ;;
        --dry-run) dry_run=1 ;;
        --regular|--major)
            [[ "$release_type" == "nightly" ]] \
                || fail "only one release type may be given"
            release_type="${argument#--}"
            ;;
        -h|--help) usage; exit 0 ;;
        -*) fail "unknown flag: $argument" ;;
        *)
            [[ "$release_type" == "nightly" ]] \
                || fail "unexpected extra argument: $argument"
            [[ "$argument" =~ $SEMVER_RE ]] \
                || fail "explicit version must be strict X.Y.Z, got: $argument"
            release_type="explicit"
            explicit_version="$argument"
            ;;
    esac
done
if [[ "$dry_run" -eq 1 && "$do_push" -eq 1 ]]; then
    fail "--dry-run and --push cannot be combined"
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" \
    || fail "run this script from a Git repository"
cd "$repo_root"

branch="$(git branch --show-current)"
[[ "$branch" == "$RELEASE_BRANCH" ]] \
    || fail "releases must be prepared from $RELEASE_BRANCH (current branch: ${branch:-detached})"
[[ -z "$(git status --porcelain)" ]] \
    || fail "working tree and index must be clean before preparing a release"
git remote get-url origin >/dev/null 2>&1 || fail "origin remote is required"

printf 'release: fetching origin/%s and tags\n' "$RELEASE_BRANCH"
git fetch --quiet --prune --tags origin
remote_ref="refs/remotes/origin/$RELEASE_BRANCH"
git show-ref --verify --quiet "$remote_ref" \
    || fail "origin/$RELEASE_BRANCH does not exist"
start_head="$(git rev-parse HEAD)"
remote_head="$(git rev-parse "$remote_ref")"
[[ "$start_head" == "$remote_head" ]] \
    || fail "$RELEASE_BRANCH must be synchronized with origin/$RELEASE_BRANCH"

current_version="$(read_package_version)" \
    || fail "could not read the [package] version from Cargo.toml"
[[ "$current_version" =~ $SEMVER_RE ]] \
    || fail "package version must be stable SemVer, got: $current_version"
latest_tag="$(latest_release_tag)" \
    || fail "no release baseline found; create and push v0.1.0 manually first"
latest_version="${latest_tag#v}"
# A nightly tag extends the package version with a fourth component; its
# three-part prefix is the base the nightly series belongs to.
if [[ "$latest_version" =~ $NIGHTLY_RE ]]; then
    latest_base="${latest_version%.*}"
else
    latest_base="$latest_version"
fi
[[ "$latest_base" == "$current_version" ]] \
    || fail "latest release tag $latest_tag does not match package version $current_version"

case "$release_type" in
    nightly)
        nightly_number=0
        if [[ "$latest_version" =~ $NIGHTLY_RE ]]; then
            nightly_number="${latest_version##*.}"
        fi
        new_version="${current_version}.$((10#$nightly_number + 1))"
        ;;
    regular) new_version="$(next_version "$current_version" patch)" ;;
    major) new_version="$(next_version "$current_version" minor)" ;;
    explicit) new_version="$(next_version "$current_version" "$explicit_version")" ;;
esac
new_tag="${TAG_PREFIX}${new_version}"
if git show-ref --verify --quiet "refs/tags/$new_tag"; then
    fail "tag $new_tag already exists"
fi
if [[ "$release_type" != "nightly" ]]; then
    # The release workflow pairs every regular/major tag with its curated
    # changelog and fails when it is missing, so require it before tagging.
    [[ -f "docs/changelogs/${new_tag}.md" ]] \
        || fail "missing changelog docs/changelogs/${new_tag}.md for $new_tag"
fi

commit_created=0
tag_created=0
restore_release_state() {
    local status=$?
    trap - EXIT INT TERM HUP
    if [[ "$tag_created" -eq 1 ]]; then
        git tag --delete "$new_tag" >/dev/null 2>&1 || true
    fi
    if [[ "$commit_created" -eq 1 ]]; then
        git reset --soft "$start_head" >/dev/null 2>&1 || true
    fi
    git restore --staged -- "${RELEASE_FILES[@]}" >/dev/null 2>&1 || true
    git restore --worktree -- "${RELEASE_FILES[@]}" >/dev/null 2>&1 || true
    exit "$status"
}
trap restore_release_state EXIT
trap 'exit 130' INT TERM HUP

printf 'release: preparing %s -> %s (%s)\n' "$current_version" "$new_version" "$release_type"
if [[ "$release_type" != "nightly" ]]; then
    write_package_version "$new_version"
    # Full resolution, not --no-deps: only then does cargo rewrite Cargo.lock
    # with the bumped package version, which the --locked release gates require.
    cargo metadata --format-version 1 >/dev/null
    # The published schema meta embeds the package version, so the bump makes
    # the checked-in files stale; regenerate before the drift-test gate runs.
    cargo run --locked --features dev-tools --bin generate-api-schema
    git add -- "${RELEASE_FILES[@]}"
fi

# These are the tracked equivalent of the release-gate checks. For regular and
# major releases the release files are staged first so the release commit is
# exactly what passed; nightlies tag HEAD, so the gates check that state.
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
# Tests must see the same non-interactive stdin as CI: interactivity checks
# (io::stdin().is_terminal()) otherwise prompt and hang under a terminal run.
cargo test --locked --all-targets --all-features </dev/null

if [[ "$dry_run" -eq 1 ]]; then
    if [[ "$release_type" == "nightly" ]]; then
        printf 'release: dry run passed; would tag HEAD as %s\n' "$new_tag"
    else
        printf 'release: dry run passed; would commit and tag %s\n' "$new_tag"
    fi
    git restore --staged -- "${RELEASE_FILES[@]}"
    git restore --worktree -- "${RELEASE_FILES[@]}"
    trap - EXIT INT TERM HUP
    exit 0
fi

if [[ "$release_type" != "nightly" ]]; then
    git commit --quiet -m "${COMMIT_PREFIX}${new_version}" -- "${RELEASE_FILES[@]}"
    commit_created=1
fi
git tag "$new_tag"
tag_created=1

# From this point the local commit (if any) and tag are intentional release
# state. A failed network push keeps them available for an explicit retry.
trap - EXIT INT TERM HUP
if [[ "$release_type" == "nightly" ]]; then
    printf 'release: created nightly tag %s\n' "$new_tag"
else
    printf 'release: created commit and tag %s\n' "$new_tag"
fi
if [[ "$do_push" -eq 1 ]]; then
    if [[ "$release_type" == "nightly" ]]; then
        if ! git push origin "refs/tags/$new_tag"; then
            printf 'release: push failed; local tag %s was kept for retry\n' "$new_tag" >&2
            exit 1
        fi
        printf 'release: pushed %s\n' "$new_tag"
    else
        if ! git push --atomic origin "$RELEASE_BRANCH" "refs/tags/$new_tag"; then
            printf 'release: push failed; local commit and tag %s were kept for retry\n' "$new_tag" >&2
            exit 1
        fi
        printf 'release: pushed %s and %s\n' "$RELEASE_BRANCH" "$new_tag"
    fi
else
    if [[ "$release_type" == "nightly" ]]; then
        printf 'release: next: git push origin refs/tags/%s\n' "$new_tag"
    else
        printf 'release: next: git push --atomic origin %s refs/tags/%s\n' \
            "$RELEASE_BRANCH" "$new_tag"
    fi
fi
