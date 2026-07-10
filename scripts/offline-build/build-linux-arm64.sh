#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build the Git AI Linux ARM64 musl binary.

Environment:
  GIT_AI_BUILD_OFFLINE=1   Require a preloaded builder image and Cargo cache.
  GIT_AI_BUILD_ROOT=PATH   Override build/offline-build.
EOF
    exit 0
fi

TARGET=aarch64-unknown-linux-musl
PLATFORM=linux/arm64
ARTIFACT=git-ai-linux-arm64

prepare_build_dirs
ensure_linux_builder "${PLATFORM}"

OUT_DIR="${ARTIFACT_ROOT}/linux"
CARGO_CACHE="${CACHE_ROOT}/cargo-${TARGET}"
TARGET_DIR="/workspace/build/offline-build/work/${TARGET}"
mkdir -p "${OUT_DIR}" "${CARGO_CACHE}"

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
    -v "${REPO_ROOT}:/workspace" \
    -v "${CARGO_CACHE}:/cargo" \
    -v "${OUT_DIR}:/output" \
    "${LINUX_BUILDER_IMAGE}" \
    sh -c "set -eu; cargo build --locked --release --target ${TARGET} --bin git-ai; strip \"${TARGET_DIR}/${TARGET}/release/git-ai\"; cp \"${TARGET_DIR}/${TARGET}/release/git-ai\" /output/${ARTIFACT}; chmod 755 /output/${ARTIFACT}; file /output/${ARTIFACT}; /output/${ARTIFACT} --version"
