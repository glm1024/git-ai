#!/bin/sh

set -eu

: "${SCRIPT_DIR:?SCRIPT_DIR must be set before sourcing common.sh}"

REPO_ROOT=$(CDPATH= cd "${SCRIPT_DIR}/../.." && pwd)
BUILD_ROOT=${GIT_AI_BUILD_ROOT:-"${REPO_ROOT}/build/offline-build"}
ARTIFACT_ROOT="${BUILD_ROOT}/artifacts"
CACHE_ROOT="${BUILD_ROOT}/cache"
WORK_ROOT="${BUILD_ROOT}/work"
RUST_VERSION=${GIT_AI_RUST_VERSION:-1.93.0}
CONTAINER_PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

info() {
    printf '%s\n' "[offline-build] $*"
}

fail() {
    printf '%s\n' "[offline-build] ERROR: $*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "Required command not found: $1"
}

require_file() {
    [ -f "$1" ] || fail "Required file not found: $1"
}

prepare_build_dirs() {
    mkdir -p "${ARTIFACT_ROOT}" "${CACHE_ROOT}" "${WORK_ROOT}"
}

is_offline_build() {
    case "${GIT_AI_BUILD_OFFLINE:-0}" in
        1|true|TRUE|yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

cargo_offline_env() {
    if is_offline_build; then
        printf '%s' true
    else
        printf '%s' false
    fi
}

cli_version() {
    awk -F '"' '/^[[:space:]]*version = / { print $2; exit }' "${REPO_ROOT}/Cargo.toml"
}

vscode_version() {
    node -p "require('${REPO_ROOT}/agent-support/vscode/package.json').version"
}

jetbrains_version() {
    awk -F ' = ' '/^pluginVersion = / { print $2; exit }' "${REPO_ROOT}/agent-support/intellij/gradle.properties"
}

linux_builder_image() {
    platform_arch=${1#linux/}
    if [ -n "${GIT_AI_LINUX_BUILDER_IMAGE:-}" ]; then
        printf '%s' "${GIT_AI_LINUX_BUILDER_IMAGE}-${platform_arch}"
    else
        printf '%s' "git-ai-offline-linux-builder:rust-${RUST_VERSION}-${platform_arch}"
    fi
}

ensure_linux_builder() {
    build_platform=$1
    LINUX_BUILDER_IMAGE=$(linux_builder_image "${build_platform}")

    require_command docker
    if docker image inspect "${LINUX_BUILDER_IMAGE}" >/dev/null 2>&1; then
        return
    fi

    if is_offline_build; then
        fail "Linux builder image is missing in offline mode: ${LINUX_BUILDER_IMAGE}"
    fi

    info "Creating Linux builder image ${LINUX_BUILDER_IMAGE} for ${build_platform}"
    docker buildx build \
        --platform "${build_platform}" \
        --load \
        --build-arg "RUST_VERSION=${RUST_VERSION}" \
        --tag "${LINUX_BUILDER_IMAGE}" \
        --file "${SCRIPT_DIR}/Dockerfile.linux-builder" \
        "${SCRIPT_DIR}"
}

ensure_windows_builder() {
    WINDOWS_BUILDER_IMAGE=${GIT_AI_WINDOWS_BUILDER_IMAGE:-"git-ai-offline-windows-builder:rust-${RUST_VERSION}-xwin-0.23.0"}

    require_command docker
    if docker image inspect "${WINDOWS_BUILDER_IMAGE}" >/dev/null 2>&1; then
        return
    fi

    if is_offline_build; then
        fail "Windows builder image is missing in offline mode: ${WINDOWS_BUILDER_IMAGE}"
    fi

    info "Creating Windows x64 cross-builder image ${WINDOWS_BUILDER_IMAGE}"
    docker buildx build \
        --platform linux/amd64 \
        --load \
        --build-arg "RUST_VERSION=${RUST_VERSION}" \
        --build-arg "CARGO_XWIN_VERSION=0.23.0" \
        --tag "${WINDOWS_BUILDER_IMAGE}" \
        --file "${SCRIPT_DIR}/Dockerfile.windows-builder" \
        "${SCRIPT_DIR}"
}

safe_git_dirty_state() {
    if [ -n "$(git -C "${REPO_ROOT}" status --porcelain)" ]; then
        printf '%s' true
    else
        printf '%s' false
    fi
}
