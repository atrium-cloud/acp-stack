#!/usr/bin/env bash

# Prepare an acp-stack release commit and tag. The tag triggers the GitHub
# Actions release workflow, which builds and publishes the artifacts; this
# script only advances the version, pairs it with its changelog, and tags.

set -euo pipefail

# Release-policy constants live here so version and Git behavior are not split
# across shell call sites.
readonly DEFAULT_BUMP="patch"
readonly RELEASE_BRANCH="main"
readonly TAG_PREFIX="v"
readonly COMMIT_PREFIX="chore: release v"
readonly SEMVER_RE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
readonly RELEASE_FILES=(Cargo.toml Cargo.lock)

usage() {
    cat <<'EOF'
Prepare an acp-stack release commit and tag.

Usage:
  scripts/release.sh [patch|minor|major|X.Y.Z] [--dry-run] [--push]

The bump defaults to patch. A curated changelog must already exist at
docs/changelogs/vX.Y.Z.md for the new version. Without --push, the release
commit and tag remain local.
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

latest_release_tag() {
    local tag
    while IFS= read -r tag; do
        if [[ "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
            printf '%s\n' "$tag"
            return 0
        fi
    done < <(git tag --list 'v*' --sort=-version:refname)
    return 1
}

bump=""
do_push=0
dry_run=0
for argument in "$@"; do
    case "$argument" in
        --push) do_push=1 ;;
        --dry-run) dry_run=1 ;;
        -h|--help) usage; exit 0 ;;
        -*) fail "unknown flag: $argument" ;;
        *)
            [[ -z "$bump" ]] || fail "unexpected extra argument: $argument"
            bump="$argument"
            ;;
    esac
done
bump="${bump:-$DEFAULT_BUMP}"
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
current_tag="${TAG_PREFIX}${current_version}"
latest_tag="$(latest_release_tag)" \
    || fail "no release baseline found; create and push v0.1.0 manually first"
[[ "$latest_tag" == "$current_tag" ]] \
    || fail "latest release tag $latest_tag does not match package version $current_version"

new_version="$(next_version "$current_version" "$bump")"
new_tag="${TAG_PREFIX}${new_version}"
if git show-ref --verify --quiet "refs/tags/$new_tag"; then
    fail "tag $new_tag already exists"
fi
# The release workflow pairs every tag with its curated changelog and fails
# when it is missing, so require it before tagging.
[[ -f "docs/changelogs/${new_tag}.md" ]] \
    || fail "missing changelog docs/changelogs/${new_tag}.md for $new_tag"

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

printf 'release: preparing %s -> %s\n' "$current_version" "$new_version"
write_package_version "$new_version"
# Full resolution, not --no-deps: only then does cargo rewrite Cargo.lock
# with the bumped package version, which the --locked release gates require.
cargo metadata --format-version 1 >/dev/null
git add -- "${RELEASE_FILES[@]}"

# These are the tracked equivalent of the release-gate checks. The release
# files are staged first so the release commit is exactly what passed.
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features

if [[ "$dry_run" -eq 1 ]]; then
    printf 'release: dry run passed; would commit and tag %s\n' "$new_tag"
    git restore --staged -- "${RELEASE_FILES[@]}"
    git restore --worktree -- "${RELEASE_FILES[@]}"
    trap - EXIT INT TERM HUP
    exit 0
fi

git commit --quiet -m "${COMMIT_PREFIX}${new_version}" -- "${RELEASE_FILES[@]}"
commit_created=1
git tag "$new_tag"
tag_created=1

# From this point the local commit and tag are intentional release state. A
# failed network push keeps them available for an explicit retry.
trap - EXIT INT TERM HUP
printf 'release: created commit and tag %s\n' "$new_tag"
if [[ "$do_push" -eq 1 ]]; then
    if ! git push --atomic origin "$RELEASE_BRANCH" "refs/tags/$new_tag"; then
        printf 'release: push failed; local commit and tag %s were kept for retry\n' "$new_tag" >&2
        exit 1
    fi
    printf 'release: pushed %s and %s\n' "$RELEASE_BRANCH" "$new_tag"
else
    printf 'release: next: git push --atomic origin %s refs/tags/%s\n' \
        "$RELEASE_BRANCH" "$new_tag"
fi
