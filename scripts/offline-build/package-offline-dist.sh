#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Package previously built CLI binaries and IDE extensions as an offline bundle.

The destination is offline-dist/git-ai-offline-v<CLI version>. Existing output
for the same version is replaced after the new bundle is assembled successfully.
EOF
    exit 0
fi

require_command awk
require_command shasum
prepare_build_dirs

CLI_VERSION=$(cli_version)
VSCODE_VERSION=$(vscode_version)
JETBRAINS_VERSION=$(jetbrains_version)
OFFLINE_VERSION=${GIT_AI_OFFLINE_VERSION:-"${CLI_VERSION}"}
DIST_NAME="git-ai-offline-v${OFFLINE_VERSION}"
DIST_DIR="${REPO_ROOT}/offline-dist/${DIST_NAME}"
STAGING_DIR="${BUILD_ROOT}/package/${DIST_NAME}.tmp.$$"

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

rm -rf "${STAGING_DIR}"
mkdir -p "${STAGING_DIR}/linux" "${STAGING_DIR}/macos" "${STAGING_DIR}/windows" "${STAGING_DIR}/vscode" "${STAGING_DIR}/jetbrains"

cleanup() {
    rm -rf "${STAGING_DIR}"
}
trap cleanup 0 HUP INT TERM

cp "${LINUX_DIR}/git-ai-linux-x64" "${STAGING_DIR}/linux/"
cp "${LINUX_DIR}/git-ai-linux-arm64" "${STAGING_DIR}/linux/"
cp "${MACOS_DIR}/git-ai-macos-arm64" "${STAGING_DIR}/macos/"
cp "${WINDOWS_DIR}/git-ai-windows-x64.exe" "${STAGING_DIR}/windows/"
cp "${VSCODE_DIR}/${VSCODE_VSIX}" "${STAGING_DIR}/vscode/"
cp "${JETBRAINS_DIR}/${JETBRAINS_ZIP}" "${STAGING_DIR}/jetbrains/"

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

EMBEDDED_CHECKSUMS=$(tr '\n' '|' < "${BIN_CHECKSUMS}" | sed 's/|$//')

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

awk -v version="v${OFFLINE_VERSION}" -v vsix="${VSCODE_VSIX}" -v jetbrains="${JETBRAINS_ZIP}" '
    {
        gsub(/v[0-9][0-9.]*/, version)
        gsub(/git-ai\.git-ai-vscode-[0-9.]+\.vsix/, vsix)
        gsub(/Git_AI-[0-9.]+\.zip/, jetbrains)
        print
    }
' "${REPO_ROOT}/offline-dist/git-ai-offline-v1.6.12/INSTALL.md" > "${STAGING_DIR}/INSTALL.md"

(
    cd "${STAGING_DIR}/linux"
    shasum -a 256 git-ai-linux-arm64 git-ai-linux-x64
    cd "${STAGING_DIR}/macos"
    shasum -a 256 git-ai-macos-arm64
    cd "${STAGING_DIR}/windows"
    shasum -a 256 git-ai-windows-x64.exe
    cd "${STAGING_DIR}/vscode"
    shasum -a 256 "${VSCODE_VSIX}"
    cd "${STAGING_DIR}/jetbrains"
    shasum -a 256 "${JETBRAINS_ZIP}"
    cd "${STAGING_DIR}"
    shasum -a 256 install.sh install.ps1
) | LC_ALL=C sort > "${STAGING_DIR}/SHA256SUMS"

{
    printf 'source_commit=%s\n' "$(git -C "${REPO_ROOT}" rev-parse HEAD)"
    printf 'source_dirty=%s\n' "$(safe_git_dirty_state)"
    printf 'cli_version=%s\n' "${CLI_VERSION}"
    printf 'vscode_version=%s\n' "${VSCODE_VERSION}"
    printf 'jetbrains_version=%s\n' "${JETBRAINS_VERSION}"
    printf 'built_at_utc=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
} > "${STAGING_DIR}/BUILD-METADATA.txt"

if [ -e "${DIST_DIR}" ]; then
    rm -rf "${DIST_DIR}"
fi
mv "${STAGING_DIR}" "${DIST_DIR}"
trap - 0 HUP INT TERM

info "Created offline bundle: ${DIST_DIR}"
