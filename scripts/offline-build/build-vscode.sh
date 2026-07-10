#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build the offline VS Code / Cursor VSIX extension.

Environment:
  GIT_AI_BUILD_OFFLINE=1   Run npm ci from the local npm cache only.
  GIT_AI_BUILD_ROOT=PATH   Override build/offline-build.
EOF
    exit 0
fi

require_command node
require_command npm
prepare_build_dirs

VSCODE_DIR="${REPO_ROOT}/agent-support/vscode"
NPM_CACHE="${CACHE_ROOT}/npm"
OUT_DIR="${ARTIFACT_ROOT}/vscode"
VSCODE_VERSION=$(vscode_version)
VSIX_NAME="git-ai.git-ai-vscode-${VSCODE_VERSION}.vsix"
VSIX_SOURCE="git-ai-vscode-${VSCODE_VERSION}.vsix"
mkdir -p "${NPM_CACHE}" "${OUT_DIR}"

info "Installing VS Code extension dependencies"
if is_offline_build; then
    (cd "${VSCODE_DIR}" && npm ci --offline --cache "${NPM_CACHE}")
else
    (cd "${VSCODE_DIR}" && npm ci --prefer-offline --cache "${NPM_CACHE}")
fi

info "Building ${VSIX_NAME}"
(cd "${VSCODE_DIR}" && npm run lint && npm run package)
require_file "${VSCODE_DIR}/${VSIX_SOURCE}"
cp "${VSCODE_DIR}/${VSIX_SOURCE}" "${OUT_DIR}/${VSIX_NAME}"
