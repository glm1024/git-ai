#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build both IDE plugins and copy them to plugins/.

This runs:
  build-vscode.sh
  build-jetbrains.sh

Then copies the VS Code / Cursor VSIX and JetBrains ZIP to:
  <repo>/plugins/

Environment:
  GIT_AI_BUILD_OFFLINE=1   Run npm/Gradle from the local caches only.
  GIT_AI_BUILD_ROOT=PATH   Override build/offline-build.
EOF
    exit 0
fi

sh "${SCRIPT_DIR}/build-vscode.sh"
sh "${SCRIPT_DIR}/build-jetbrains.sh"

VSCODE_VERSION=$(vscode_version)
JETBRAINS_VERSION=$(jetbrains_version)
VSIX_NAME="git-ai.git-ai-vscode-${VSCODE_VERSION}.vsix"
ZIP_NAME="Git_AI-${JETBRAINS_VERSION}.zip"
VSIX_PATH="${ARTIFACT_ROOT}/vscode/${VSIX_NAME}"
ZIP_PATH="${ARTIFACT_ROOT}/jetbrains/${ZIP_NAME}"
PLUGIN_DIR="${REPO_ROOT}/plugins"

require_file "${VSIX_PATH}"
require_file "${ZIP_PATH}"

mkdir -p "${PLUGIN_DIR}"
# Replace previous local copies so version bumps do not leave stale packages.
rm -f "${PLUGIN_DIR}/git-ai.git-ai-vscode-"*.vsix
rm -f "${PLUGIN_DIR}/Git_AI-"*.zip
cp "${VSIX_PATH}" "${PLUGIN_DIR}/${VSIX_NAME}"
cp "${ZIP_PATH}" "${PLUGIN_DIR}/${ZIP_NAME}"

info "Copied ${VSIX_NAME} and ${ZIP_NAME} to ${PLUGIN_DIR}"
