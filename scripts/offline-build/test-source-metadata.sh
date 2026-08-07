#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

require_command awk
require_command mktemp
require_command shasum

for safe_version in 1.6.16 release_1-rc.2 A; do
    is_safe_release_version "${safe_version}" \
        || fail "Safe release version was rejected: ${safe_version}"
done

for unsafe_version in '' . .. ../escape x/../../escape 'x\..\escape' -leading .hidden 'x y'; do
    if is_safe_release_version "${unsafe_version}"; then
        fail "Unsafe release version was accepted: ${unsafe_version}"
    fi
done

TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/git-ai-source-metadata.XXXXXX")
ARTIFACT_DIR="${TEST_ROOT}/artifact path"
ARTIFACT="${ARTIFACT_DIR}/dummy artifact.bin"
METADATA="${ARTIFACT}.build-metadata"
EXPECTED_COMMIT=$(git -C "${REPO_ROOT}" rev-parse HEAD)

cleanup_test() {
    rm -rf "${TEST_ROOT}"
}
trap cleanup_test 0 HUP INT TERM

mkdir -p "${ARTIFACT_DIR}"
printf 'known artifact bytes\n' > "${ARTIFACT}"
EXPECTED_SHA=$(shasum -a 256 "${ARTIFACT}" | awk '{print $1}')

write_metadata() {
    metadata_format=$1
    artifact_name=$2
    artifact_sha=$3
    source_commit=$4
    source_dirty=$5
    {
        printf 'format=%s\n' "${metadata_format}"
        printf 'artifact_name=%s\n' "${artifact_name}"
        printf 'artifact_sha256=%s\n' "${artifact_sha}"
        printf 'source_commit=%s\n' "${source_commit}"
        printf 'source_dirty=%s\n' "${source_dirty}"
        printf 'built_at_utc=2026-01-01T00:00:00Z\n'
    } > "${METADATA}"
}

write_valid_metadata() {
    write_metadata \
        git-ai-offline-artifact-v1 \
        "$(basename "${ARTIFACT}")" \
        "${EXPECTED_SHA}" \
        "${EXPECTED_COMMIT}" \
        false
}

expect_rejected() {
    label=$1
    if (validate_artifact_source_metadata "${ARTIFACT}" "${EXPECTED_COMMIT}") \
        >/dev/null 2>&1
    then
        fail "Source metadata test unexpectedly accepted: ${label}"
    fi
}

write_valid_metadata
validate_artifact_source_metadata "${ARTIFACT}" "${EXPECTED_COMMIT}"

rm -f "${METADATA}"
expect_rejected "missing metadata"

write_metadata git-ai-offline-artifact-v1 "$(basename "${ARTIFACT}")" "${EXPECTED_SHA}" stale-commit false
expect_rejected "stale commit"

write_metadata git-ai-offline-artifact-v1 "$(basename "${ARTIFACT}")" "${EXPECTED_SHA}" "${EXPECTED_COMMIT}" true
expect_rejected "dirty source"

write_metadata git-ai-offline-artifact-v1 "$(basename "${ARTIFACT}")" invalid-sha "${EXPECTED_COMMIT}" false
expect_rejected "checksum mismatch"

write_metadata git-ai-offline-artifact-v1 "$(basename "${ARTIFACT}")" "" "${EXPECTED_COMMIT}" false
expect_rejected "empty checksum"

write_metadata unsupported-format "$(basename "${ARTIFACT}")" "${EXPECTED_SHA}" "${EXPECTED_COMMIT}" false
expect_rejected "unsupported format"

write_metadata git-ai-offline-artifact-v1 wrong-name "${EXPECTED_SHA}" "${EXPECTED_COMMIT}" false
expect_rejected "artifact name mismatch"

write_valid_metadata
printf 'source_commit=%s\n' "${EXPECTED_COMMIT}" >> "${METADATA}"
expect_rejected "duplicate metadata key"

write_valid_metadata
printf 'tampered bytes\n' >> "${ARTIFACT}"
expect_rejected "artifact replaced after metadata was written"

FAKE_BIN="${TEST_ROOT}/fake bin"
mkdir -p "${FAKE_BIN}"
printf '#!/bin/sh\nexit 7\n' > "${FAKE_BIN}/shasum"
chmod 755 "${FAKE_BIN}/shasum"
ORIGINAL_PATH=${PATH}
PATH="${FAKE_BIN}:${PATH}"
if sha256_file "${ARTIFACT}" >/dev/null 2>&1; then
    fail "SHA-256 command failure was swallowed"
fi
PATH=${ORIGINAL_PATH}

printf '#!/bin/sh\nexit 9\n' > "${FAKE_BIN}/git"
chmod 755 "${FAKE_BIN}/git"
PATH="${FAKE_BIN}:${PATH}"
if safe_git_dirty_state >/dev/null 2>&1; then
    fail "Git source-state query failure was reduced to a clean tree"
fi
PATH=${ORIGINAL_PATH}

info "Source metadata fail-closed tests passed"
