#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build the Git AI Windows x64 MSVC executable using cargo-xwin in Docker.

The first online build downloads and caches the Microsoft SDK and CRT. A real
Windows x64 machine must still run the final installation and attribution smoke
test before release.

Environment:
  GIT_AI_BUILD_OFFLINE=1   Require a preloaded builder image and xwin cache.
  GIT_AI_BUILD_ROOT=PATH   Override build/offline-build.
EOF
    exit 0
fi

TARGET=x86_64-pc-windows-msvc
PLATFORM=linux/amd64
ARTIFACT=git-ai-windows-x64.exe

prepare_build_dirs
ensure_windows_builder

OUT_DIR="${ARTIFACT_ROOT}/windows"
CARGO_CACHE="${CACHE_ROOT}/cargo-${TARGET}"
XWIN_CACHE="${CACHE_ROOT}/xwin"
MARKER_FILE="${XWIN_CACHE}/.git-ai-xwin-x64-ready"
TARGET_DIR="/workspace/build/offline-build/work/${TARGET}"
mkdir -p "${OUT_DIR}" "${CARGO_CACHE}" "${XWIN_CACHE}"

if [ ! -f "${MARKER_FILE}" ]; then
    if is_offline_build; then
        fail "Windows SDK/CRT cache is missing. Run this script once with network access before offline builds."
    fi

    info "Caching Windows x64 SDK and CRT for cargo-xwin"
    docker run --rm \
        --platform "${PLATFORM}" \
        --user "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e PATH="${CONTAINER_PATH}" \
        -e CARGO_HOME=/cargo \
        -e XWIN_ARCH=x86_64 \
        -e XWIN_CACHE_DIR=/xwin-cache \
        -v "${CARGO_CACHE}:/cargo" \
        -v "${XWIN_CACHE}:/xwin-cache" \
        "${WINDOWS_BUILDER_IMAGE}" \
        sh -c 'cargo xwin cache xwin'
    touch "${MARKER_FILE}"
fi

info "Building ${ARTIFACT}"
docker run --rm \
    --platform "${PLATFORM}" \
    -w /workspace \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e PATH="${CONTAINER_PATH}" \
    -e CARGO_HOME=/cargo \
    -e CARGO_NET_OFFLINE="$(cargo_offline_env)" \
    -e CARGO_TARGET_DIR="${TARGET_DIR}" \
    -e OSS_BUILD=1 \
    -e XWIN_ARCH=x86_64 \
    -e XWIN_CACHE_DIR=/xwin-cache \
    -v "${REPO_ROOT}:/workspace" \
    -v "${CARGO_CACHE}:/cargo" \
    -v "${XWIN_CACHE}:/xwin-cache" \
    -v "${OUT_DIR}:/output" \
    "${WINDOWS_BUILDER_IMAGE}" \
    sh -c "set -eu; cargo xwin build --locked --release --target ${TARGET} --bin git-ai; cp \"${TARGET_DIR}/${TARGET}/release/git-ai.exe\" /output/${ARTIFACT}; file /output/${ARTIFACT}"
