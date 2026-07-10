#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build the offline JetBrains plugin ZIP.

Environment:
  GIT_AI_BUILD_OFFLINE=1   Require the Gradle wrapper and dependency cache.
  GIT_AI_BUILD_ROOT=PATH   Override build/offline-build.
EOF
    exit 0
fi

require_command java
prepare_build_dirs

JETBRAINS_DIR="${REPO_ROOT}/agent-support/intellij"
OUT_DIR="${ARTIFACT_ROOT}/jetbrains"
GRADLE_CACHE="${CACHE_ROOT}/gradle"
JETBRAINS_VERSION=$(jetbrains_version)
SOURCE_ZIP_NAME="Git AI-${JETBRAINS_VERSION}.zip"
ZIP_NAME="Git_AI-${JETBRAINS_VERSION}.zip"
mkdir -p "${OUT_DIR}" "${GRADLE_CACHE}"

info "Building ${ZIP_NAME}"
if is_offline_build; then
    (cd "${JETBRAINS_DIR}" && GRADLE_USER_HOME="${GRADLE_CACHE}" ./gradlew --no-daemon --offline buildPlugin)
else
    (cd "${JETBRAINS_DIR}" && GRADLE_USER_HOME="${GRADLE_CACHE}" ./gradlew --no-daemon buildPlugin)
fi

require_file "${JETBRAINS_DIR}/build/distributions/${SOURCE_ZIP_NAME}"
cp "${JETBRAINS_DIR}/build/distributions/${SOURCE_ZIP_NAME}" "${OUT_DIR}/${ZIP_NAME}"
