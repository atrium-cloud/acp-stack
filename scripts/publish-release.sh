#!/usr/bin/env bash

# Publish a built release matrix to a release hosting service. Runs in the
# tag-triggered release workflow with an OIDC-capable job (id-token: write).
# The service verifies the workflow identity against its publisher trust
# policy, issues checksum- and size-bound presigned uploads, and finalizes the
# release only after every stored object matches the manifest.
#
# Usage:
#   scripts/publish-release.sh <manifest-path> <product> <version-tag>
#
# Requires the GitHub Actions OIDC environment (ACTIONS_ID_TOKEN_REQUEST_URL
# and ACTIONS_ID_TOKEN_REQUEST_TOKEN) plus three configuration variables:
# PUBLISH_BASE_URL (service origin), PUBLISH_OIDC_AUDIENCE (OIDC token
# audience), and PUBLISH_RUNTIME_PROFILE (manifest runtime profile).
# Publication is idempotent: an identical retry returns the existing session,
# finalizing an already-published session returns the existing release, and a
# rerun (a strictly newer run attempt) supersedes a pending session left by a
# failed upload, so re-running a failed workflow job is the supported retry
# path.

set -euo pipefail

# Stable tags are vMAJOR.MINOR.PATCH; nightly tags add a fourth component that
# exists only in the tag and packaging names (the Cargo.toml version stays at
# the three-part base).
readonly RELEASE_TAG_RE='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(\.(0|[1-9][0-9]*))?$'
# Only this target is served from the release hosting service; the other
# built targets ship via the GitHub Release alone.
readonly PUBLISH_TARGET="x86_64-unknown-linux-gnu"

usage() {
    cat <<'EOF'
Publish a built release matrix to a release hosting service.

Usage:
  scripts/publish-release.sh <manifest-path> <product> <version-tag>

Example:
  scripts/publish-release.sh dist/acps-release.json acp-stack v0.1.5
EOF
}

log() {
    printf 'publish-release: %s\n' "$*" >&2
}

fail() {
    printf 'publish-release: error: %s\n' "$*" >&2
    exit 1
}

mint_token() {
    local response curl_status=0
    response="$(
        curl --fail-with-body --silent --show-error \
            --header "Authorization: bearer ${ACTIONS_ID_TOKEN_REQUEST_TOKEN}" \
            "${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=${PUBLISH_OIDC_AUDIENCE}"
    )" || curl_status=$?
    if [[ "$curl_status" -ne 0 ]]; then
        # Only an HTTP-error body (curl exit 22) is safe to print. A
        # transport failure after a 200 (partial transfer, timeout, reset)
        # leaves a truncated token body in $response, and nothing has
        # ::add-mask::ed it yet.
        if [[ "$curl_status" -eq 22 ]]; then
            fail "OIDC token request was rejected: $response"
        fi
        fail "OIDC token request failed (curl exit $curl_status)"
    fi
    # A successful body carries the token itself, so never echo it on the
    # parse-failure path either.
    jq --exit-status --raw-output '.value' <<<"$response" \
        || fail "OIDC token response was not understood"
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi
[[ "$#" -eq 3 ]] || { usage; exit 1; }

manifest_path="$1"
product="$2"
version_tag="$3"

[[ -f "$manifest_path" ]] || fail "manifest not found: $manifest_path"
[[ "$version_tag" =~ $RELEASE_TAG_RE ]] \
    || fail "version tag must be a canonical vMAJOR.MINOR.PATCH tag (optionally with a nightly .N suffix), got: $version_tag"
[[ -n "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" && -n "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ]] \
    || fail "GitHub OIDC environment is missing; the job needs 'id-token: write' permission"
[[ -n "${PUBLISH_BASE_URL:-}" ]] || fail "PUBLISH_BASE_URL is not configured"
[[ -n "${PUBLISH_OIDC_AUDIENCE:-}" ]] || fail "PUBLISH_OIDC_AUDIENCE is not configured"
[[ -n "${PUBLISH_RUNTIME_PROFILE:-}" ]] || fail "PUBLISH_RUNTIME_PROFILE is not configured"
command -v jq >/dev/null 2>&1 || fail "jq is required"

dist_dir="$(cd "$(dirname "$manifest_path")" && pwd)"

# The publish manifest accepts exactly these fields. The build manifest's
# artifact entries are filtered to the published target and gain the fixed
# binary name to complete the required shape.
body="$(
    jq --compact-output --exit-status \
        --arg product "$product" \
        --arg version "$version_tag" \
        --arg runtime_profile "$PUBLISH_RUNTIME_PROFILE" \
        --arg publish_target "$PUBLISH_TARGET" \
        '{product: $product, version: $version, runtime_profile: $runtime_profile,
          artifacts: [.artifacts[] | select(.target == $publish_target) | . + {binary: "acps"}]}' \
        "$manifest_path"
)" || fail "could not derive the publish manifest from $manifest_path"
artifact_count="$(
    jq --exit-status --arg publish_target "$PUBLISH_TARGET" \
        '[.artifacts[] | select(.target == $publish_target)] | length' \
        "$manifest_path"
)" || fail "could not read the artifact list from $manifest_path"
[[ "$artifact_count" -gt 0 ]] \
    || fail "manifest has no artifacts for target $PUBLISH_TARGET: $manifest_path"

log "initiating publication of $product $version_tag ($artifact_count artifacts)"
token="$(mint_token)" || fail "could not mint a GitHub OIDC token"
printf '::add-mask::%s\n' "$token"
# On curl --fail-with-body failures the service's error body lands on stdout
# and is captured by the substitution, so carry it into the failure message;
# --show-error alone reports only the bare HTTP status.
response="$(
    curl --fail-with-body --silent --show-error \
        --request POST \
        --header "Authorization: Bearer ${token}" \
        --header 'Content-Type: application/json' \
        --data "$body" \
        "${PUBLISH_BASE_URL}/v1/publishes"
)" || fail "publish initiation was rejected: $response"

# The initiation response must never be echoed wholesale into failure
# messages: its presigned upload URLs are bearer-equivalent credentials.
publish_id="$(jq --exit-status --raw-output '.publish_id' <<<"$response")" \
    || fail "publish initiation response is missing publish_id"
state="$(jq --exit-status --raw-output '.state' <<<"$response")" \
    || fail "publish initiation response is missing state"

if [[ "$state" == "pending" ]]; then
    upload_count="$(jq --exit-status '.uploads | length' <<<"$response")" \
        || fail "publish initiation response is missing the uploads list"
    [[ "$upload_count" -eq "$artifact_count" ]] \
        || fail "service issued $upload_count uploads for $artifact_count manifest artifacts"

    for ((index = 0; index < upload_count; index++)); do
        upload="$(jq --compact-output --exit-status ".uploads[$index]" <<<"$response")" \
            || fail "upload entry $index is missing from the initiation response"
        archive="$(jq --exit-status --raw-output '.archive' <<<"$upload")" \
            || fail "upload entry $index has no archive name"
        url="$(jq --exit-status --raw-output '.url' <<<"$upload")" \
            || fail "upload entry $index ($archive) has no presigned url"
        file="$dist_dir/$archive"
        [[ -f "$file" ]] || fail "upload archive missing from dist: $archive"

        # Parse the headers before the loop that consumes them: a jq failure
        # inside a process substitution is invisible to set -e, and a PUT
        # without the issued headers would fail later with an opaque
        # storage-side error instead of here. The service always binds
        # uploads to at least the size and checksum headers, so an empty
        # set is also an error (jq --exit-status fails on empty output).
        headers_tsv="$(jq --exit-status --raw-output \
            '.headers | to_entries[] | "\(.key)\t\(.value)"' <<<"$upload")" \
            || fail "upload entry for $archive carries no headers"
        header_args=()
        while IFS=$'\t' read -r header_name header_value; do
            header_args+=(--header "$header_name: $header_value")
        done <<<"$headers_tsv"

        log "uploading $archive"
        # The presigned request is bound to the declared size and checksum;
        # send the returned headers exactly as issued.
        upload_response="$(
            curl --fail-with-body --silent --show-error \
                --request PUT \
                "${header_args[@]}" \
                --data-binary "@$file" \
                "$url"
        )" || fail "upload failed for $archive: $upload_response"
    done
elif [[ "$state" == "published" ]]; then
    # Identical retry of an already-finalized publication: nothing to upload.
    log "publication $publish_id is already published; skipping uploads"
else
    fail "unexpected publication state: $state"
fi

# Uploads may outlive the first token, so mint a fresh one for finalization.
token="$(mint_token)" || fail "could not mint a GitHub OIDC token for finalization"
printf '::add-mask::%s\n' "$token"
log "finalizing publication $publish_id"
finalize="$(
    curl --fail-with-body --silent --show-error \
        --request POST \
        --header "Authorization: Bearer ${token}" \
        "${PUBLISH_BASE_URL}/v1/publishes/${publish_id}/finalize"
)" || fail "publication finalization failed: $finalize"

jq --exit-status --raw-output \
    '"published \(.product) \(.version): release_id=\(.release_id) advanced_latest=\(.advanced_latest)"' \
    <<<"$finalize" >&2 || fail "finalization response was not understood: $finalize"
