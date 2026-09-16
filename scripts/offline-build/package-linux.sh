#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Package previously built Linux x64 and ARM64 CLI binaries for offline use.

The final deliverable is:
  offline-dist/git-ai-linux-v<CLI version>.tar.gz

The packaged installer detects the Linux architecture and verifies the selected
binary internally. Users do not run a separate checksum command.
EOF
    exit 0
fi

require_command awk
require_command grep
require_command shasum
require_command tar
prepare_build_dirs
require_clean_release_source

PACKAGE_SOURCE_COMMIT=$(git -C "${REPO_ROOT}" rev-parse HEAD)
CLI_VERSION=$(cli_version)
require_safe_release_version "${CLI_VERSION}"

LINUX_DIR="${ARTIFACT_ROOT}/linux"
X64_ARTIFACT="${LINUX_DIR}/git-ai-linux-x64"
ARM64_ARTIFACT="${LINUX_DIR}/git-ai-linux-arm64"
DIST_NAME="git-ai-linux-v${CLI_VERSION}"
DIST_ARCHIVE="${REPO_ROOT}/offline-dist/${DIST_NAME}.tar.gz"
STAGING_ROOT="${REPO_ROOT}/offline-dist/.git-ai-linux-package-${DIST_NAME}.$$"
STAGING_DIR="${STAGING_ROOT}/${DIST_NAME}"
STAGING_ARCHIVE="${REPO_ROOT}/offline-dist/.git-ai-linux-package-${DIST_NAME}.$$.tar.gz"

validate_artifact_source_metadata "${X64_ARTIFACT}" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${ARM64_ARTIFACT}" "${PACKAGE_SOURCE_COMMIT}"

cleanup() {
    rm -rf "${STAGING_ROOT}"
    rm -f "${STAGING_ARCHIVE}"
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

mkdir -p "${STAGING_DIR}/linux"
cp "${X64_ARTIFACT}" "${STAGING_DIR}/linux/"
cp "${X64_ARTIFACT}.build-metadata" "${STAGING_DIR}/linux/"
cp "${ARM64_ARTIFACT}" "${STAGING_DIR}/linux/"
cp "${ARM64_ARTIFACT}.build-metadata" "${STAGING_DIR}/linux/"

validate_artifact_source_metadata \
    "${STAGING_DIR}/linux/git-ai-linux-x64" \
    "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata \
    "${STAGING_DIR}/linux/git-ai-linux-arm64" \
    "${PACKAGE_SOURCE_COMMIT}"

BIN_CHECKSUMS="${STAGING_ROOT}/embedded-checksums"
(
    cd "${STAGING_DIR}/linux"
    shasum -a 256 git-ai-linux-arm64 git-ai-linux-x64
) > "${BIN_CHECKSUMS}"
EMBEDDED_CHECKSUMS=$(
    awk '
        NF {
            if (count > 0) printf "|"
            printf "%s", $0
            count += 1
        }
        END { if (count != 2) exit 1 }
    ' "${BIN_CHECKSUMS}"
) || fail "Could not embed both Linux binary checksums"

awk \
    -v repo="internal/git-ai-linux-offline" \
    -v version="v${CLI_VERSION}" \
    -v checksums="${EMBEDDED_CHECKSUMS}" '
    /^REPO="/ { print "REPO=\"" repo "\""; next }
    /^PINNED_VERSION="/ { print "PINNED_VERSION=\"" version "\""; next }
    /^EMBEDDED_CHECKSUMS="/ { print "EMBEDDED_CHECKSUMS=\"" checksums "\""; next }
    /^BUNDLED_BINARY_DIR="/ { print "BUNDLED_BINARY_DIR=\"linux\""; next }
    /^BUNDLED_TARGET_OS="/ { print "BUNDLED_TARGET_OS=\"linux\""; next }
    { print }
' "${REPO_ROOT}/install.sh" > "${STAGING_DIR}/install.sh"
chmod 755 "${STAGING_DIR}/install.sh"
cp "${SCRIPT_DIR}/LINUX-INSTALL.md" "${STAGING_DIR}/INSTALL.md"

PACKAGED_AT_UTC=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
{
    printf 'bundle_format=git-ai-linux-offline-v1\n'
    printf 'source_commit=%s\n' "${PACKAGE_SOURCE_COMMIT}"
    printf 'source_dirty=false\n'
    printf 'artifact_source_gate=verified-local-v1\n'
    printf 'cli_version=%s\n' "${CLI_VERSION}"
    printf 'architectures=x86_64,aarch64\n'
    printf 'built_at_utc=%s\n' "${PACKAGED_AT_UTC}"
    printf 'packaged_at_utc=%s\n' "${PACKAGED_AT_UTC}"
} > "${STAGING_DIR}/BUILD-METADATA.txt"

# Keep a package manifest for release audit. The installer performs the
# selected-binary check automatically, so users do not invoke this file.
(
    cd "${STAGING_DIR}"
    shasum -a 256 \
        linux/git-ai-linux-x64 \
        linux/git-ai-linux-x64.build-metadata \
        linux/git-ai-linux-arm64 \
        linux/git-ai-linux-arm64.build-metadata \
        install.sh \
        INSTALL.md \
        BUILD-METADATA.txt \
        | LC_ALL=C sort > SHA256SUMS
    shasum -a 256 -c SHA256SUMS >/dev/null
)

require_unchanged_release_source "${PACKAGE_SOURCE_COMMIT}"
(
    cd "${STAGING_ROOT}"
    tar -czf "${STAGING_ARCHIVE}" "${DIST_NAME}"
)
tar -tzf "${STAGING_ARCHIVE}" >/dev/null \
    || fail "Generated Linux archive failed integrity validation"
tar -tzf "${STAGING_ARCHIVE}" | grep -Fxq "${DIST_NAME}/install.sh" \
    || fail "Generated Linux archive is missing ${DIST_NAME}/install.sh"
tar -tzf "${STAGING_ARCHIVE}" | grep -Fxq "${DIST_NAME}/linux/git-ai-linux-x64" \
    || fail "Generated Linux archive is missing the x64 CLI"
tar -tzf "${STAGING_ARCHIVE}" | grep -Fxq "${DIST_NAME}/linux/git-ai-linux-arm64" \
    || fail "Generated Linux archive is missing the ARM64 CLI"

mv -f "${STAGING_ARCHIVE}" "${DIST_ARCHIVE}"
trap - 0 HUP INT TERM
cleanup

info "Created Linux offline archive: ${DIST_ARCHIVE}"
