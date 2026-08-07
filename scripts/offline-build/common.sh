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

is_safe_release_version() {
    version_value=$1
    [ -n "${version_value}" ] || return 1
    case "${version_value}" in
        .|..) return 1 ;;
        [0-9A-Za-z]*) ;;
        *) return 1 ;;
    esac
    case "${version_value}" in
        *[!0-9A-Za-z._-]*) return 1 ;;
        *) return 0 ;;
    esac
}

require_safe_release_version() {
    version_value=$1
    is_safe_release_version "${version_value}" \
        || fail "Unsafe offline release version: ${version_value}. Use one path segment containing only letters, numbers, dots, underscores, and hyphens."
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
    if ! git_status_output=$(git -C "${REPO_ROOT}" status --porcelain); then
        printf '%s\n' "[offline-build] ERROR: Could not inspect source tree state." >&2
        return 1
    fi

    if [ -n "${git_status_output}" ]; then
        printf '%s' true
    else
        printf '%s' false
    fi
}

is_sha256() {
    sha_value=$1
    [ "${#sha_value}" -eq 64 ] || return 1
    case "${sha_value}" in
        *[!0-9a-fA-F]*) return 1 ;;
        *) return 0 ;;
    esac
}

sha256_file() {
    sha_path=$1
    if ! sha_output=$(shasum -a 256 "${sha_path}"); then
        printf '%s\n' "[offline-build] ERROR: Could not hash artifact: ${sha_path}" >&2
        return 1
    fi

    sha_value=${sha_output%% *}
    if ! is_sha256 "${sha_value}"; then
        printf '%s\n' "[offline-build] ERROR: Invalid SHA-256 output for artifact: ${sha_path}" >&2
        return 1
    fi
    printf '%s' "${sha_value}"
}

begin_artifact_build() {
    artifact_path=$1
    require_command git
    require_command awk
    require_command shasum

    # Invalidate any prior source record before the build starts. If the build
    # fails, an old artifact may remain for debugging but cannot be packaged.
    rm -f "${artifact_path}.build-metadata"
    BUILD_SOURCE_COMMIT=$(git -C "${REPO_ROOT}" rev-parse HEAD)
    BUILD_SOURCE_DIRTY=$(safe_git_dirty_state)
}

finish_artifact_build() {
    artifact_path=$1
    require_file "${artifact_path}"

    : "${BUILD_SOURCE_COMMIT:?begin_artifact_build must run before building an artifact}"
    : "${BUILD_SOURCE_DIRTY:?begin_artifact_build must run before building an artifact}"

    current_commit=$(git -C "${REPO_ROOT}" rev-parse HEAD)
    if [ "${current_commit}" != "${BUILD_SOURCE_COMMIT}" ]; then
        fail "Source commit changed while building $(basename "${artifact_path}"); rebuild the artifact."
    fi

    current_dirty=$(safe_git_dirty_state)
    if [ "${current_dirty}" != "${BUILD_SOURCE_DIRTY}" ]; then
        fail "Source tree changed while building $(basename "${artifact_path}"); rebuild the artifact."
    fi

    artifact_sha256=$(sha256_file "${artifact_path}") \
        || fail "Could not record artifact checksum: ${artifact_path}"
    metadata_path="${artifact_path}.build-metadata"
    metadata_tmp="${metadata_path}.tmp.$$"
    {
        printf 'format=git-ai-offline-artifact-v1\n'
        printf 'artifact_name=%s\n' "$(basename "${artifact_path}")"
        printf 'artifact_sha256=%s\n' "${artifact_sha256}"
        printf 'source_commit=%s\n' "${BUILD_SOURCE_COMMIT}"
        printf 'source_dirty=%s\n' "${BUILD_SOURCE_DIRTY}"
        printf 'built_at_utc=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    } > "${metadata_tmp}"
    mv "${metadata_tmp}" "${metadata_path}"
}

metadata_value() {
    metadata_path=$1
    metadata_key=$2
    awk -F= -v key="${metadata_key}" '
        $1 == key {
            count += 1
            value = substr($0, length(key) + 2)
        }
        END {
            if (count != 1) exit 1
            print value
        }
    ' "${metadata_path}"
}

validate_artifact_source_metadata() {
    artifact_path=$1
    expected_commit=$2
    metadata_path="${artifact_path}.build-metadata"

    require_file "${artifact_path}"
    require_file "${metadata_path}"

    recorded_format=$(metadata_value "${metadata_path}" format) \
        || fail "Missing or duplicate format in ${metadata_path}"
    recorded_name=$(metadata_value "${metadata_path}" artifact_name) \
        || fail "Missing or duplicate artifact_name in ${metadata_path}"
    recorded_sha256=$(metadata_value "${metadata_path}" artifact_sha256) \
        || fail "Missing or duplicate artifact_sha256 in ${metadata_path}"
    recorded_commit=$(metadata_value "${metadata_path}" source_commit) \
        || fail "Missing or duplicate source_commit in ${metadata_path}"
    recorded_dirty=$(metadata_value "${metadata_path}" source_dirty) \
        || fail "Missing or duplicate source_dirty in ${metadata_path}"
    recorded_built_at=$(metadata_value "${metadata_path}" built_at_utc) \
        || fail "Missing or duplicate built_at_utc in ${metadata_path}"

    [ "${recorded_format}" = git-ai-offline-artifact-v1 ] \
        || fail "Unsupported artifact source metadata format in ${metadata_path}: ${recorded_format}"
    [ "${recorded_name}" = "$(basename "${artifact_path}")" ] \
        || fail "Artifact name does not match source metadata: ${artifact_path}"
    [ "${recorded_commit}" = "${expected_commit}" ] \
        || fail "Stale or mixed artifact $(basename "${artifact_path}"): built from ${recorded_commit}, expected ${expected_commit}."
    [ "${recorded_dirty}" = false ] \
        || fail "Artifact $(basename "${artifact_path}") was built from a dirty source tree."
    [ -n "${recorded_built_at}" ] \
        || fail "Empty built_at_utc in ${metadata_path}"
    is_sha256 "${recorded_sha256}" \
        || fail "Invalid artifact_sha256 in ${metadata_path}"

    actual_sha256=$(sha256_file "${artifact_path}") \
        || fail "Could not verify artifact checksum: ${artifact_path}"
    [ "${recorded_sha256}" = "${actual_sha256}" ] \
        || fail "Artifact checksum does not match source metadata: ${artifact_path}"
}

require_clean_release_source() {
    require_command git
    if [ "$(safe_git_dirty_state)" != false ]; then
        fail "Offline release packaging requires a clean source tree. Commit or stash source changes, rebuild every artifact, then package again."
    fi
}

require_unchanged_release_source() {
    expected_commit=$1
    current_commit=$(git -C "${REPO_ROOT}" rev-parse HEAD)
    [ "${current_commit}" = "${expected_commit}" ] \
        || fail "Source commit changed during offline release packaging."
    require_clean_release_source
}
