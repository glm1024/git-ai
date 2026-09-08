#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build and package the Windows x64 CLI for offline installation.

The final deliverable is:
  offline-dist/git-ai-windows-v<CLI version>.zip
EOF
    exit 0
fi

require_command grep
require_command shasum
require_command unzip
require_command zip
require_clean_release_source

sh "${SCRIPT_DIR}/build-windows-x64.sh"

PACKAGE_SOURCE_COMMIT=$(git -C "${REPO_ROOT}" rev-parse HEAD)
CLI_VERSION=$(cli_version)
require_safe_release_version "${CLI_VERSION}"

ARTIFACT="${ARTIFACT_ROOT}/windows/git-ai-windows-x64.exe"
METADATA="${ARTIFACT}.build-metadata"
DIST_NAME="git-ai-windows-v${CLI_VERSION}"
DIST_ZIP="${REPO_ROOT}/offline-dist/${DIST_NAME}.zip"
STAGING_ROOT="${REPO_ROOT}/offline-dist/.git-ai-windows-package-${DIST_NAME}.$$"
STAGING_DIR="${STAGING_ROOT}/${DIST_NAME}"
STAGING_ZIP="${REPO_ROOT}/offline-dist/.git-ai-windows-package-${DIST_NAME}.$$.zip"

require_file "${ARTIFACT}"
require_file "${METADATA}"
validate_artifact_source_metadata "${ARTIFACT}" "${PACKAGE_SOURCE_COMMIT}"

cleanup() {
    rm -rf "${STAGING_ROOT}"
    rm -f "${STAGING_ZIP}"
}

abort_on_signal() {
    signal_status=$1
    trap - 0 HUP INT TERM
    cleanup
    exit "${signal_status}"
}

cleanup
trap cleanup 0
trap 'abort_on_signal 129' HUP
trap 'abort_on_signal 130' INT
trap 'abort_on_signal 143' TERM

mkdir -p "${STAGING_DIR}/windows"
cp "${ARTIFACT}" "${STAGING_DIR}/windows/"
cp "${METADATA}" "${STAGING_DIR}/windows/"
cp "${REPO_ROOT}/install.ps1" "${STAGING_DIR}/install.ps1"
cp "${SCRIPT_DIR}/WINDOWS-INSTALL.md" "${STAGING_DIR}/INSTALL.md"

validate_artifact_source_metadata \
    "${STAGING_DIR}/windows/git-ai-windows-x64.exe" \
    "${PACKAGE_SOURCE_COMMIT}"

PACKAGED_AT_UTC=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
{
    printf 'bundle_format=git-ai-windows-offline-v1\n'
    printf 'source_commit=%s\n' "${PACKAGE_SOURCE_COMMIT}"
    printf 'source_dirty=false\n'
    printf 'artifact_source_gate=verified-local-v1\n'
    printf 'cli_version=%s\n' "${CLI_VERSION}"
    printf 'built_at_utc=%s\n' "${PACKAGED_AT_UTC}"
    printf 'packaged_at_utc=%s\n' "${PACKAGED_AT_UTC}"
} > "${STAGING_DIR}/BUILD-METADATA.txt"

(
    cd "${STAGING_DIR}"
    shasum -a 256 \
        windows/git-ai-windows-x64.exe \
        windows/git-ai-windows-x64.exe.build-metadata \
        install.ps1 \
        INSTALL.md \
        BUILD-METADATA.txt \
        | LC_ALL=C sort > SHA256SUMS
    shasum -a 256 -c SHA256SUMS >/dev/null
)

require_unchanged_release_source "${PACKAGE_SOURCE_COMMIT}"
(
    cd "${STAGING_ROOT}"
    zip -q -r "${STAGING_ZIP}" "${DIST_NAME}"
)
unzip -tq "${STAGING_ZIP}" >/dev/null \
    || fail "Generated Windows ZIP failed integrity validation"
unzip -Z1 "${STAGING_ZIP}" | grep -Fxq "${DIST_NAME}/SHA256SUMS" \
    || fail "Generated Windows ZIP is missing ${DIST_NAME}/SHA256SUMS"

mv -f "${STAGING_ZIP}" "${DIST_ZIP}"
trap - 0 HUP INT TERM
cleanup

info "Created Windows offline ZIP: ${DIST_ZIP}"
