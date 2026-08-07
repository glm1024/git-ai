#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Package previously built CLI binaries and IDE extensions as an offline bundle.

The destination is offline-dist/git-ai-offline-v<CLI version>. Existing output
for the same version is replaced after the new bundle is assembled successfully.
Every artifact must have local build metadata from a clean build of the current
source commit; stale, dirty, mixed, or modified artifacts are rejected.
EOF
    exit 0
fi

require_command awk
require_command shasum
prepare_build_dirs
require_clean_release_source

PACKAGE_SOURCE_COMMIT=$(git -C "${REPO_ROOT}" rev-parse HEAD)

CLI_VERSION=$(cli_version)
VSCODE_VERSION=$(vscode_version)
JETBRAINS_VERSION=$(jetbrains_version)
OFFLINE_VERSION=${GIT_AI_OFFLINE_VERSION:-"${CLI_VERSION}"}
require_safe_release_version "${OFFLINE_VERSION}"
DIST_NAME="git-ai-offline-v${OFFLINE_VERSION}"
DIST_DIR="${REPO_ROOT}/offline-dist/${DIST_NAME}"
STAGING_DIR="${REPO_ROOT}/offline-dist/.git-ai-package-${DIST_NAME}.staging.$$"
BACKUP_DIR=""
PREVIOUS_RELEASE_MOVED=false
NEW_RELEASE_INSTALLED=false

LINUX_DIR="${ARTIFACT_ROOT}/linux"
MACOS_DIR="${ARTIFACT_ROOT}/macos"
WINDOWS_DIR="${ARTIFACT_ROOT}/windows"
VSCODE_DIR="${ARTIFACT_ROOT}/vscode"
JETBRAINS_DIR="${ARTIFACT_ROOT}/jetbrains"
VSCODE_VSIX="git-ai.git-ai-vscode-${VSCODE_VERSION}.vsix"
JETBRAINS_ZIP="Git_AI-${JETBRAINS_VERSION}.zip"

require_file "${LINUX_DIR}/git-ai-linux-x64"
require_file "${LINUX_DIR}/git-ai-linux-arm64"
require_file "${MACOS_DIR}/git-ai-macos-arm64"
require_file "${WINDOWS_DIR}/git-ai-windows-x64.exe"
require_file "${VSCODE_DIR}/${VSCODE_VSIX}"
require_file "${JETBRAINS_DIR}/${JETBRAINS_ZIP}"

validate_artifact_source_metadata "${LINUX_DIR}/git-ai-linux-x64" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${LINUX_DIR}/git-ai-linux-arm64" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${MACOS_DIR}/git-ai-macos-arm64" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${WINDOWS_DIR}/git-ai-windows-x64.exe" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${VSCODE_DIR}/${VSCODE_VSIX}" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${JETBRAINS_DIR}/${JETBRAINS_ZIP}" "${PACKAGE_SOURCE_COMMIT}"

rm -rf "${STAGING_DIR}"
mkdir -p "${STAGING_DIR}/linux" "${STAGING_DIR}/macos" "${STAGING_DIR}/windows" "${STAGING_DIR}/vscode" "${STAGING_DIR}/jetbrains"

cleanup() {
    if [ "${PREVIOUS_RELEASE_MOVED}" = true ] \
        && [ "${NEW_RELEASE_INSTALLED}" = false ] \
        && [ -n "${BACKUP_DIR}" ] \
        && [ -e "${BACKUP_DIR}" ] \
        && [ ! -e "${DIST_DIR}" ]
    then
        if ! mv "${BACKUP_DIR}" "${DIST_DIR}"; then
            printf '%s\n' "[offline-build] ERROR: Failed to restore previous bundle from ${BACKUP_DIR}" >&2
        fi
    fi
    rm -rf "${STAGING_DIR}"
}

abort_on_signal() {
    signal_status=$1
    # A caught signal does not terminate a POSIX shell automatically. Disable
    # every trap first, perform the same rollback as an ordinary exit, and then
    # leave with the conventional signal status so packaging cannot continue.
    trap - 0 HUP INT TERM
    cleanup
    exit "${signal_status}"
}

trap cleanup 0
trap 'abort_on_signal 129' HUP
trap 'abort_on_signal 130' INT
trap 'abort_on_signal 143' TERM

cp "${LINUX_DIR}/git-ai-linux-x64" "${STAGING_DIR}/linux/"
cp "${LINUX_DIR}/git-ai-linux-x64.build-metadata" "${STAGING_DIR}/linux/"
cp "${LINUX_DIR}/git-ai-linux-arm64" "${STAGING_DIR}/linux/"
cp "${LINUX_DIR}/git-ai-linux-arm64.build-metadata" "${STAGING_DIR}/linux/"
cp "${MACOS_DIR}/git-ai-macos-arm64" "${STAGING_DIR}/macos/"
cp "${MACOS_DIR}/git-ai-macos-arm64.build-metadata" "${STAGING_DIR}/macos/"
cp "${WINDOWS_DIR}/git-ai-windows-x64.exe" "${STAGING_DIR}/windows/"
cp "${WINDOWS_DIR}/git-ai-windows-x64.exe.build-metadata" "${STAGING_DIR}/windows/"
cp "${VSCODE_DIR}/${VSCODE_VSIX}" "${STAGING_DIR}/vscode/"
cp "${VSCODE_DIR}/${VSCODE_VSIX}.build-metadata" "${STAGING_DIR}/vscode/"
cp "${JETBRAINS_DIR}/${JETBRAINS_ZIP}" "${STAGING_DIR}/jetbrains/"
cp "${JETBRAINS_DIR}/${JETBRAINS_ZIP}.build-metadata" "${STAGING_DIR}/jetbrains/"

# Verify the copied bytes rather than trusting the live artifact after a
# preflight check. This closes the check-to-copy gap and keeps the source record
# in the final bundle for later audit.
validate_artifact_source_metadata "${STAGING_DIR}/linux/git-ai-linux-x64" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${STAGING_DIR}/linux/git-ai-linux-arm64" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${STAGING_DIR}/macos/git-ai-macos-arm64" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${STAGING_DIR}/windows/git-ai-windows-x64.exe" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${STAGING_DIR}/vscode/${VSCODE_VSIX}" "${PACKAGE_SOURCE_COMMIT}"
validate_artifact_source_metadata "${STAGING_DIR}/jetbrains/${JETBRAINS_ZIP}" "${PACKAGE_SOURCE_COMMIT}"

BIN_CHECKSUMS="${BUILD_ROOT}/package/${DIST_NAME}.embedded-checksums"
mkdir -p "$(dirname "${BIN_CHECKSUMS}")"
(
    cd "${STAGING_DIR}/linux"
    shasum -a 256 git-ai-linux-arm64 git-ai-linux-x64
    cd "${STAGING_DIR}/macos"
    shasum -a 256 git-ai-macos-arm64
    cd "${STAGING_DIR}/windows"
    shasum -a 256 git-ai-windows-x64.exe
) > "${BIN_CHECKSUMS}"

EMBEDDED_CHECKSUMS=$(
    awk '
        NF {
            if (count > 0) printf "|"
            printf "%s", $0
            count += 1
        }
        END { if (count != 4) exit 1 }
    ' "${BIN_CHECKSUMS}"
) || fail "Could not embed the complete four-binary checksum set"
[ -n "${EMBEDDED_CHECKSUMS}" ] || fail "Embedded binary checksum set is empty"

awk -v repo="internal/git-ai-offline" -v version="v${OFFLINE_VERSION}" -v checksums="${EMBEDDED_CHECKSUMS}" '
    /^REPO="/ { print "REPO=\"" repo "\""; next }
    /^PINNED_VERSION="/ { print "PINNED_VERSION=\"" version "\""; next }
    /^EMBEDDED_CHECKSUMS="/ { print "EMBEDDED_CHECKSUMS=\"" checksums "\""; next }
    { print }
' "${REPO_ROOT}/install.sh" > "${STAGING_DIR}/install.sh"
chmod 755 "${STAGING_DIR}/install.sh"

awk -v repo="internal/git-ai-offline" -v version="v${OFFLINE_VERSION}" -v checksums="${EMBEDDED_CHECKSUMS}" '
    /^\$Repo = / { print "$Repo = \047" repo "\047"; next }
    /^\$PinnedVersion = / { print "$PinnedVersion = \047" version "\047"; next }
    /^\$EmbeddedChecksums = / { print "$EmbeddedChecksums = \047" checksums "\047"; next }
    { print }
' "${REPO_ROOT}/install.ps1" > "${STAGING_DIR}/install.ps1"

INSTALL_TEMPLATE=${GIT_AI_INSTALL_TEMPLATE:-}
if [ -z "${INSTALL_TEMPLATE}" ] && [ -f "${DIST_DIR}/INSTALL.md" ]; then
    INSTALL_TEMPLATE="${DIST_DIR}/INSTALL.md"
fi
if [ -z "${INSTALL_TEMPLATE}" ]; then
    INSTALL_TEMPLATE=$(
        find "${REPO_ROOT}/offline-dist" -mindepth 2 -maxdepth 2 -type f -name INSTALL.md -print 2>/dev/null \
            | LC_ALL=C sort \
            | tail -n 1
    ) || true
fi
if [ -z "${INSTALL_TEMPLATE}" ] || [ ! -f "${INSTALL_TEMPLATE}" ]; then
    fail "INSTALL.md template not found under offline-dist/. Set GIT_AI_INSTALL_TEMPLATE or keep a previous offline bundle."
fi

awk -v version="v${OFFLINE_VERSION}" -v vsix="${VSCODE_VSIX}" -v jetbrains="${JETBRAINS_ZIP}" '
    {
        gsub(/v[0-9][0-9.]*/, version)
        gsub(/git-ai\.git-ai-vscode-[0-9.]+\.vsix/, vsix)
        gsub(/Git_AI-[0-9.]+\.zip/, jetbrains)
        print
    }
' "${INSTALL_TEMPLATE}" > "${STAGING_DIR}/INSTALL.md"

RAW_SHA256SUMS="${STAGING_DIR}/.SHA256SUMS.raw"
(
    cd "${STAGING_DIR}"
    shasum -a 256 \
        linux/git-ai-linux-arm64 \
        linux/git-ai-linux-arm64.build-metadata \
        linux/git-ai-linux-x64 \
        linux/git-ai-linux-x64.build-metadata \
        macos/git-ai-macos-arm64 \
        macos/git-ai-macos-arm64.build-metadata \
        windows/git-ai-windows-x64.exe \
        windows/git-ai-windows-x64.exe.build-metadata \
        "vscode/${VSCODE_VSIX}" \
        "vscode/${VSCODE_VSIX}.build-metadata" \
        "jetbrains/${JETBRAINS_ZIP}" \
        "jetbrains/${JETBRAINS_ZIP}.build-metadata" \
        install.sh \
        install.ps1
) > "${RAW_SHA256SUMS}"
LC_ALL=C sort "${RAW_SHA256SUMS}" > "${STAGING_DIR}/SHA256SUMS"
rm -f "${RAW_SHA256SUMS}"

{
    printf 'source_commit=%s\n' "${PACKAGE_SOURCE_COMMIT}"
    printf 'source_dirty=false\n'
    printf 'artifact_source_gate=verified-local-v1\n'
    printf 'cli_version=%s\n' "${CLI_VERSION}"
    printf 'vscode_version=%s\n' "${VSCODE_VERSION}"
    printf 'jetbrains_version=%s\n' "${JETBRAINS_VERSION}"
    PACKAGED_AT_UTC=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    printf 'built_at_utc=%s\n' "${PACKAGED_AT_UTC}"
    printf 'packaged_at_utc=%s\n' "${PACKAGED_AT_UTC}"
} > "${STAGING_DIR}/BUILD-METADATA.txt"

require_unchanged_release_source "${PACKAGE_SOURCE_COMMIT}"

BACKUP_DIR="${REPO_ROOT}/offline-dist/.git-ai-package-${DIST_NAME}.backup.$$"
[ ! -e "${BACKUP_DIR}" ] || fail "Temporary release backup already exists: ${BACKUP_DIR}"

if [ -e "${DIST_DIR}" ]; then
    PREVIOUS_RELEASE_MOVED=true
    if ! mv "${DIST_DIR}" "${BACKUP_DIR}"; then
        PREVIOUS_RELEASE_MOVED=false
        fail "Could not preserve previous offline bundle before replacement: ${DIST_DIR}"
    fi
fi

if ! mv "${STAGING_DIR}" "${DIST_DIR}"; then
    if [ "${PREVIOUS_RELEASE_MOVED}" = true ]; then
        mv "${BACKUP_DIR}" "${DIST_DIR}" \
            || fail "New bundle install failed and previous bundle could not be restored; recover it from ${BACKUP_DIR}"
        PREVIOUS_RELEASE_MOVED=false
    fi
    fail "Could not install completed offline bundle: ${DIST_DIR}"
fi
NEW_RELEASE_INSTALLED=true

if [ "${PREVIOUS_RELEASE_MOVED}" = true ]; then
    rm -rf "${BACKUP_DIR}"
    PREVIOUS_RELEASE_MOVED=false
fi
trap - 0 HUP INT TERM

info "Created offline bundle: ${DIST_DIR}"
