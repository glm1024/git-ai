#!/bin/bash

# This script uses Bash-only syntax below. When an operator invokes it via
# `sh install.sh`, re-exec in a non-POSIX Bash before the shell reaches those
# constructs. macOS /bin/sh is Bash in POSIX mode, so BASH_VERSION alone is not
# a sufficient guard.
NEED_BASH_REEXEC=false
if [ -z "${BASH_VERSION:-}" ] || [ -n "${POSIXLY_CORRECT:-}" ]; then
    NEED_BASH_REEXEC=true
fi
case "${BASH:-}" in
    */sh|sh) NEED_BASH_REEXEC=true ;;
esac
case ":${SHELLOPTS:-}:" in
    *:posix:*) NEED_BASH_REEXEC=true ;;
esac
if [ "$NEED_BASH_REEXEC" = true ]; then
    unset POSIXLY_CORRECT
    if command -v bash >/dev/null 2>&1; then
        exec bash "$0" "$@"
    fi
    printf '%s\n' 'Error: install.sh requires Bash.' >&2
    exit 1
fi

set -euo pipefail
IFS=$'\n\t'

print_usage() {
    cat <<'EOF'
Install git-ai for the current user.

Usage: bash install.sh
       sh install.sh

Options:
  -h, --help  Show this help without downloading or changing local files.
EOF
}

if [ "$#" -gt 0 ]; then
    if [ "$#" -eq 1 ] && { [ "$1" = -h ] || [ "$1" = --help ]; }; then
        print_usage
        exit 0
    fi
    printf 'Error: unknown installer argument:' >&2
    printf ' %s' "$@" >&2
    printf '\n' >&2
    print_usage >&2
    exit 2
fi

# ============================================================
# Ensure HOME is set when running via MDMs (e.g. JAMF) or other environments where HOME may be unbound.
# ============================================================
INSTALL_USER=""

if [ -z "${HOME:-}" ]; then
    if command -v scutil >/dev/null 2>&1; then
        CURRENT_USER=$( /usr/sbin/scutil <<< "show State:/Users/ConsoleUser" | awk '/Name :/ { print $3 }' || true )
        if [ -n "${CURRENT_USER:-}" ] && [ "$CURRENT_USER" != "loginwindow" ] && [ "$CURRENT_USER" != "_mbsetupuser" ]; then
            export HOME=$( /usr/bin/dscl . -read "/Users/$CURRENT_USER" NFSHomeDirectory | awk '{print $2}' )
            INSTALL_USER="$CURRENT_USER"
        else
            echo "Error: No console user logged in. Deferring installation." >&2
            exit 1
        fi
    elif id -un >/dev/null 2>&1; then
        INSTALL_USER="$(id -un)"
        export HOME=$(getent passwd "$INSTALL_USER" | cut -d: -f6)
        if [ -z "$HOME" ]; then
            export HOME="/root"
        fi
    else
        export HOME="/root"
    fi
fi

# Ensure SHELL is set (also may be unbound in JAMF)
if [ -z "${SHELL:-}" ]; then
    if command -v zsh >/dev/null 2>&1; then
        SHELL="$(command -v zsh)"
    elif command -v bash >/dev/null 2>&1; then
        SHELL="$(command -v bash)"
    else
        SHELL="/bin/sh"
    fi
    export SHELL
fi

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# GitHub repository details
# Replaced during release builds with the actual repository (e.g., "git-ai-project/git-ai")
# When set to __REPO_PLACEHOLDER__, defaults to "git-ai-project/git-ai"
REPO="__REPO_PLACEHOLDER__"
if [ "$REPO" = "__REPO_PLACEHOLDER__" ]; then
    REPO="git-ai-project/git-ai"
fi

# Version placeholder - replaced during release builds with actual version (e.g., "v1.0.24")
# When set to __VERSION_PLACEHOLDER__, defaults to "latest"
PINNED_VERSION="__VERSION_PLACEHOLDER__"

# Embedded checksums - replaced during release builds with actual SHA256 checksums
# Format: "hash  filename|hash  filename|..." (pipe-separated)
# When set to __CHECKSUMS_PLACEHOLDER__, checksum verification is skipped
EMBEDDED_CHECKSUMS="__CHECKSUMS_PLACEHOLDER__"

# Function to print error messages
error() {
    echo -e "${RED}Error: $1${NC}" >&2
    exit 1
}

warn() {
    echo -e "${YELLOW}Warning: $1${NC}" >&2
}

# Function to print success messages
success() {
    echo -e "${GREEN}$1${NC}"
}

# Function to verify checksum of downloaded binary
verify_checksum() {
    local file="$1"
    local binary_name="$2"

    # Skip verification if no checksums are embedded
    if [ "$EMBEDDED_CHECKSUMS" = "__CHECKSUMS_PLACEHOLDER__" ]; then
        return 0
    fi

    # Extract expected checksum for this binary
    local expected=""
    local old_ifs="$IFS"
    IFS='|' read -ra CHECKSUM_ENTRIES <<< "$EMBEDDED_CHECKSUMS"
    IFS="$old_ifs"
    for entry in "${CHECKSUM_ENTRIES[@]}"; do
        if [[ "$entry" =~ ^[[:xdigit:]]+[[:space:]]+$binary_name$ ]]; then
            expected=$(echo "$entry" | awk '{print $1}')
            break
        fi
    done

    if [ -z "$expected" ]; then
        error "No checksum found for $binary_name"
    fi

    # Calculate actual checksum
    local actual=""
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$file" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$file" | awk '{print $1}')
    else
        rm -f "$file" 2>/dev/null || true
        error "Cannot verify $binary_name: neither sha256sum nor shasum is available"
    fi

    if [ "$expected" != "$actual" ]; then
        rm -f "$file" 2>/dev/null || true
        error "Checksum verification failed for $binary_name\nExpected: $expected\nActual:   $actual"
    fi

    success "Checksum verified for $binary_name"
}

normalize_version() {
    printf '%s\n' "$1" | awk '
        match($0, /[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)*/) {
            print substr($0, RSTART, RLENGTH)
            exit
        }
    '
}

version_is_greater() {
    awk -v left="$1" -v right="$2" 'BEGIN {
        left_count = split(left, left_parts, ".")
        right_count = split(right, right_parts, ".")
        count = left_count > right_count ? left_count : right_count
        for (i = 1; i <= count; i++) {
            left_value = (i <= left_count ? left_parts[i] : 0) + 0
            right_value = (i <= right_count ? right_parts[i] : 0) + 0
            if (left_value > right_value) exit 0
            if (left_value < right_value) exit 1
        }
        exit 1
    }'
}

# ============================================================
# Warn when installing as root/sudo (not recommended).
# Running as root creates files that normal-user processes
# cannot access, causing persistent daemon lock failures.
# ============================================================
if [ "$(id -u)" = "0" ] && [ "${GIT_AI_ALLOW_SUPERUSER:-}" != "1" ]; then
    # Auto-allow in CI environments, MDM deployments (JAMF, etc.),
    # and daemon-triggered self-updates (GIT_AI_DAEMON_UPGRADE is set internally by the upgrade command)
    IS_CI_OR_MDM=false
    if [ -n "${CI:-}" ] || [ -n "${GITHUB_ACTIONS:-}" ] || [ -n "${GITLAB_CI:-}" ] \
        || [ -n "${JENKINS_URL:-}" ] || [ -n "${BUILDKITE:-}" ] || [ -n "${CIRCLECI:-}" ] \
        || [ -n "${CODEBUILD_BUILD_ID:-}" ] || [ -n "${AGENT_OS:-}" ] \
        || [ -n "${KUBERNETES_SERVICE_HOST:-}" ] || [ -n "${INSTALL_USER:-}" ] \
        || [ -n "${GIT_AI_DAEMON_UPGRADE:-}" ] \
        || [ -n "${container:-}" ] || [ -f "/.dockerenv" ]; then
        IS_CI_OR_MDM=true
    fi

    if [ "$IS_CI_OR_MDM" = "false" ]; then
        echo ""
        echo -e "${YELLOW}Warning: installing git-ai as root/sudo is not recommended.${NC}"
        echo ""
        echo "Running with elevated privileges creates files owned by root that become"
        echo "inaccessible to your normal user account, causing persistent daemon lock"
        echo "failures. A future version may refuse to install in this configuration."
        echo ""
        echo "To suppress this warning, either:"
        echo "  - Run this installer as your normal user (recommended), or"
        echo "  - Set GIT_AI_ALLOW_SUPERUSER=1"
        echo ""
    fi
    # Propagate to child git-ai invocations (install-hooks, exchange-nonce, login)
    export GIT_AI_ALLOW_SUPERUSER=1
fi

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)


# Map architecture to binary name
case $ARCH in
    "x86_64")
        ARCH="x64"
        ;;
    "aarch64"|"arm64")
        ARCH="arm64"
        ;;
    *)
        error "Unsupported architecture: $ARCH"
        ;;
esac

# Map OS to binary name
case $OS in
    "darwin")
        OS="macos"
        ;;
    "linux")
        OS="linux"
        ;;
    *)
        error "Unsupported operating system: $OS"
        ;;
esac

# Determine binary name
BINARY_NAME="git-ai-${OS}-${ARCH}"

# Determine release tag
# Priority: 1. Local binary override, 2. Pinned version (for release builds), 3. Environment variable, 4. "latest"
if [ -n "${GIT_AI_LOCAL_BINARY:-}" ]; then
    RELEASE_TAG="local"
    DOWNLOAD_URL=""
elif [ "$PINNED_VERSION" != "__VERSION_PLACEHOLDER__" ]; then
    # Version-pinned install script from a release
    RELEASE_TAG="$PINNED_VERSION"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
elif [ -n "${GIT_AI_RELEASE_TAG:-}" ] && [ "${GIT_AI_RELEASE_TAG:-}" != "latest" ]; then
    # Environment variable override
    RELEASE_TAG="$GIT_AI_RELEASE_TAG"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
else
    # Default to latest
    RELEASE_TAG="latest"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}"
fi

# Install into the user's bin directory ~/.git-ai/bin. Executable publication
# uses stable backup names plus a durable journal. A later installer run can
# therefore recover a transaction interrupted by SIGKILL, host restart, or
# power loss without touching configuration, SQLite files, or outbox data.
INSTALL_ROOT="$HOME/.git-ai"
INSTALL_DIR="${INSTALL_ROOT}/bin"
FINAL_BINARY="${INSTALL_DIR}/git-ai"
GIT_SHIM="${INSTALL_DIR}/git"
LOCAL_ROOT="$HOME/.local"
LOCAL_BIN_DIR="${LOCAL_ROOT}/bin"
CLI_LINK="${LOCAL_BIN_DIR}/git-ai"
STAGING_DIR="${INSTALL_DIR}/.git-ai.install-staged"
TMP_FILE="${STAGING_DIR}/git-ai"
BINARY_BACKUP="${FINAL_BINARY}.install-backup"
GIT_SHIM_BACKUP="${GIT_SHIM}.install-backup"
CLI_LINK_BACKUP="${CLI_LINK}.install-backup"
INSTALL_JOURNAL="${INSTALL_ROOT}/install-transaction"
INSTALL_JOURNAL_TMP="${INSTALL_JOURNAL}.tmp.$$"
INSTALL_LOCK_DIR="${INSTALL_ROOT}/install.lock.d"
INSTALL_LOCK_PID="${INSTALL_LOCK_DIR}/pid"

INSTALL_ROOT_CREATED=false
INSTALL_DIR_CREATED=false
LOCAL_ROOT_CREATED=false
LOCAL_BIN_DIR_CREATED=false
INSTALL_LOCK_HELD=false
BINARY_PRESERVED=false
GIT_SHIM_PRESERVED=false
CLI_LINK_PRESERVED=false
BINARY_PUBLISHED=false
GIT_SHIM_PUBLISHED=false
CLI_LINK_PUBLISHED=false
BINARY_WAS_PRESENT=false
GIT_SHIM_WAS_PRESENT=false
CLI_LINK_WAS_PRESENT=false
INSTALL_TRANSACTION_ACTIVE=false
PREVIOUS_VERSION=""

path_exists() {
    [ -e "$1" ] || [ -L "$1" ]
}

remove_staged_candidate() {
    rm -f "$TMP_FILE" 2>/dev/null || true
    rmdir "$STAGING_DIR" 2>/dev/null || true
}

release_install_lock() {
    if [ "$INSTALL_LOCK_HELD" != true ]; then
        return 0
    fi
    rm -f "$INSTALL_LOCK_PID" 2>/dev/null || true
    if ! rmdir "$INSTALL_LOCK_DIR" 2>/dev/null; then
        warn "Could not remove installer lock directory: $INSTALL_LOCK_DIR"
    fi
    INSTALL_LOCK_HELD=false
}

acquire_install_lock() {
    mkdir -p "$INSTALL_ROOT"
    if ! mkdir "$INSTALL_LOCK_DIR" 2>/dev/null; then
        local owner_pid=""
        if [ -f "$INSTALL_LOCK_PID" ]; then
            owner_pid=$(awk 'NR == 1 && /^[0-9]+$/ { print; exit }' "$INSTALL_LOCK_PID" 2>/dev/null || true)
        fi
        if [ -n "$owner_pid" ] && kill -0 "$owner_pid" 2>/dev/null; then
            error "Another git-ai installer is running (PID $owner_pid)"
        fi
        rm -f "$INSTALL_LOCK_PID" 2>/dev/null || true
        if ! rmdir "$INSTALL_LOCK_DIR" 2>/dev/null || ! mkdir "$INSTALL_LOCK_DIR" 2>/dev/null; then
            error "Stale installer lock requires manual inspection: $INSTALL_LOCK_DIR"
        fi
    fi
    INSTALL_LOCK_HELD=true
    if ! (umask 077 && printf '%s\n' "$$" > "$INSTALL_LOCK_PID"); then
        error "Could not record installer lock ownership"
    fi
}

journal_value() {
    local key="$1"
    awk -F= -v key="$key" '$1 == key { print substr($0, length(key) + 2); exit }' "$INSTALL_JOURNAL"
}

write_install_journal() {
    local phase="$1"
    if ! (
        umask 077
        {
            printf 'format=1\n'
            printf 'phase=%s\n' "$phase"
            printf 'binary_was_present=%s\n' "$BINARY_WAS_PRESENT"
            printf 'git_shim_was_present=%s\n' "$GIT_SHIM_WAS_PRESENT"
            printf 'cli_link_was_present=%s\n' "$CLI_LINK_WAS_PRESENT"
        } > "$INSTALL_JOURNAL_TMP"
    ); then
        return 1
    fi
    if ! mv -f "$INSTALL_JOURNAL_TMP" "$INSTALL_JOURNAL"; then
        rm -f "$INSTALL_JOURNAL_TMP" 2>/dev/null || true
        return 1
    fi
    # Ensure the recovery record reaches stable storage before executable paths
    # are moved. `sync` has no file operand on older macOS, so use the portable
    # whole-filesystem form.
    sync
}

restore_recovered_path() {
    local final_path="$1"
    local backup_path="$2"
    local was_present="$3"
    local label="$4"

    if [ "$was_present" = true ]; then
        if path_exists "$backup_path"; then
            if path_exists "$final_path" && ! rm -f "$final_path"; then
                warn "Could not remove interrupted $label at $final_path"
                return 1
            fi
            if ! mv "$backup_path" "$final_path"; then
                warn "Could not restore $label from $backup_path"
                return 1
            fi
        else
            warn "Interrupted install is ambiguous for $label: the recovery journal says an old path existed, but no backup is present. The current path may be either old or newly published."
            return 1
        fi
    else
        if path_exists "$backup_path"; then
            warn "Unexpected $label backup requires manual inspection: $backup_path"
            return 1
        fi
        if path_exists "$final_path" && ! rm -f "$final_path"; then
            warn "Could not remove $label created by the interrupted install: $final_path"
            return 1
        fi
    fi
}

recover_interrupted_install() {
    local backup
    for backup in "$BINARY_BACKUP" "$GIT_SHIM_BACKUP" "$CLI_LINK_BACKUP"; do
        if [ -d "$backup" ] && [ ! -L "$backup" ]; then
            error "Installer backup path is a directory: $backup"
        fi
    done

    if [ ! -f "$INSTALL_JOURNAL" ]; then
        for backup in "$BINARY_BACKUP" "$GIT_SHIM_BACKUP" "$CLI_LINK_BACKUP"; do
            if path_exists "$backup"; then
                error "Installer backup exists without a recovery journal: $backup"
            fi
        done
        remove_staged_candidate
        return 0
    fi

    local journal_format journal_phase binary_was_present git_shim_was_present cli_link_was_present
    journal_format=$(journal_value format)
    journal_phase=$(journal_value phase)
    binary_was_present=$(journal_value binary_was_present)
    git_shim_was_present=$(journal_value git_shim_was_present)
    cli_link_was_present=$(journal_value cli_link_was_present)

    if [ "$journal_format" != 1 ]; then
        error "Unsupported or corrupt installer recovery journal: $INSTALL_JOURNAL"
    fi
    case "$journal_phase" in
        prepared|committed) ;;
        *) error "Unsupported or corrupt installer recovery phase: $journal_phase" ;;
    esac
    for value in "$binary_was_present" "$git_shim_was_present" "$cli_link_was_present"; do
        case "$value" in
            true|false) ;;
            *) error "Corrupt installer recovery journal: $INSTALL_JOURNAL" ;;
        esac
    done

    if [ "$journal_phase" = committed ]; then
        for backup in "$BINARY_BACKUP" "$GIT_SHIM_BACKUP" "$CLI_LINK_BACKUP"; do
            if path_exists "$backup" && ! rm -f "$backup"; then
                error "Could not finish cleanup from the previous install: $backup"
            fi
        done
    else
        local recovery_failed=false
        restore_recovered_path "$CLI_LINK" "$CLI_LINK_BACKUP" "$cli_link_was_present" "CLI link" || recovery_failed=true
        restore_recovered_path "$GIT_SHIM" "$GIT_SHIM_BACKUP" "$git_shim_was_present" "git shim" || recovery_failed=true
        restore_recovered_path "$FINAL_BINARY" "$BINARY_BACKUP" "$binary_was_present" "git-ai binary" || recovery_failed=true
        if [ "$recovery_failed" = true ]; then
            error "Could not recover the interrupted install; journal retained at $INSTALL_JOURNAL"
        fi
        success "Recovered an interrupted git-ai install before continuing"
    fi

    remove_staged_candidate
    if ! rm -f "$INSTALL_JOURNAL"; then
        error "Could not clear completed installer recovery journal: $INSTALL_JOURNAL"
    fi
}

rollback_install_transaction() {
    local original_status="${1:-1}"
    local restore_failed=false

    trap - EXIT HUP INT TERM
    set +e

    if [ "$INSTALL_TRANSACTION_ACTIVE" != true ]; then
        remove_staged_candidate
        rm -f "$INSTALL_JOURNAL_TMP" 2>/dev/null || true
        release_install_lock
        exit "$original_status"
    fi

    [ "$original_status" -ne 0 ] || original_status=1

    if path_exists "$TMP_FILE" && ! rm -f "$TMP_FILE" 2>/dev/null; then
        restore_failed=true
        warn "Failed to remove staged binary: $TMP_FILE"
    fi
    rmdir "$STAGING_DIR" 2>/dev/null || true

    if [ "$CLI_LINK_PUBLISHED" = true ]; then
        if path_exists "$CLI_LINK" && ! rm -f "$CLI_LINK" 2>/dev/null; then
            restore_failed=true
            warn "Failed to remove failed CLI link: $CLI_LINK"
        fi
    fi
    if [ "$CLI_LINK_PRESERVED" = true ]; then
        if ! mv "$CLI_LINK_BACKUP" "$CLI_LINK"; then
            restore_failed=true
            warn "Failed to restore the previous CLI link; recover it from $CLI_LINK_BACKUP"
        fi
    fi

    if [ "$GIT_SHIM_PUBLISHED" = true ]; then
        if path_exists "$GIT_SHIM" && ! rm -f "$GIT_SHIM" 2>/dev/null; then
            restore_failed=true
            warn "Failed to remove failed git shim: $GIT_SHIM"
        fi
    fi
    if [ "$GIT_SHIM_PRESERVED" = true ]; then
        if ! mv "$GIT_SHIM_BACKUP" "$GIT_SHIM"; then
            restore_failed=true
            warn "Failed to restore the previous git shim; recover it from $GIT_SHIM_BACKUP"
        fi
    fi

    if [ "$BINARY_PUBLISHED" = true ]; then
        if path_exists "$FINAL_BINARY" && ! rm -f "$FINAL_BINARY" 2>/dev/null; then
            restore_failed=true
            warn "Failed to remove failed git-ai binary: $FINAL_BINARY"
        fi
    fi
    if [ "$BINARY_PRESERVED" = true ]; then
        if ! mv "$BINARY_BACKUP" "$FINAL_BINARY"; then
            restore_failed=true
            warn "Failed to restore the previous git-ai binary; recover it from $BINARY_BACKUP"
        fi
    fi

    if [ "$restore_failed" = true ]; then
        warn "Installation failed and at least one previous executable path needs manual recovery. Preserved data under $INSTALL_ROOT was not removed."
    else
        rm -f "$INSTALL_JOURNAL" "$INSTALL_JOURNAL_TMP" 2>/dev/null || true
    fi
    INSTALL_TRANSACTION_ACTIVE=false
    release_install_lock

    if [ "$LOCAL_BIN_DIR_CREATED" = true ]; then
        rmdir "$LOCAL_BIN_DIR" 2>/dev/null || true
    fi
    if [ "$LOCAL_ROOT_CREATED" = true ]; then
        rmdir "$LOCAL_ROOT" 2>/dev/null || true
    fi
    if [ "$INSTALL_DIR_CREATED" = true ]; then
        rmdir "$INSTALL_DIR" 2>/dev/null || true
    fi
    if [ "$INSTALL_ROOT_CREATED" = true ]; then
        rmdir "$INSTALL_ROOT" 2>/dev/null || true
    fi
    exit "$original_status"
}

complete_install_transaction() {
    if ! write_install_journal committed; then
        error "Could not commit the installer recovery journal"
    fi
    INSTALL_TRANSACTION_ACTIVE=false
    trap - EXIT HUP INT TERM

    local cleanup_failed=false
    for backup in "$BINARY_BACKUP" "$GIT_SHIM_BACKUP" "$CLI_LINK_BACKUP"; do
        if path_exists "$backup" && ! rm -f "$backup"; then
            warn "Installed successfully, but could not remove obsolete backup: $backup"
            cleanup_failed=true
        fi
    done
    remove_staged_candidate
    if [ "$cleanup_failed" = false ]; then
        rm -f "$INSTALL_JOURNAL" "$INSTALL_JOURNAL_TMP" 2>/dev/null || true
    else
        warn "Committed recovery journal retained for cleanup on the next installer run: $INSTALL_JOURNAL"
    fi
    release_install_lock
}

inject_install_failure_if_requested() {
    local step="$1"
    if [ "${GIT_AI_INSTALL_TEST_FAIL_AT:-}" = "$step" ]; then
        error "Injected installer failure at $step"
    fi
}

if [ ! -d "$INSTALL_ROOT" ]; then
    INSTALL_ROOT_CREATED=true
fi
if [ ! -d "$INSTALL_DIR" ]; then
    INSTALL_DIR_CREATED=true
fi
trap 'rollback_install_transaction $?' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

acquire_install_lock
recover_interrupted_install
mkdir -p "$INSTALL_DIR"
if path_exists "$STAGING_DIR"; then
    if [ -L "$STAGING_DIR" ] || [ ! -d "$STAGING_DIR" ]; then
        error "Installer staging path requires manual inspection: $STAGING_DIR"
    fi
    remove_staged_candidate
    if path_exists "$STAGING_DIR"; then
        error "Installer staging directory is not empty: $STAGING_DIR"
    fi
fi
mkdir "$STAGING_DIR"
INSTALL_TRANSACTION_ACTIVE=true

# Download and stage the candidate before touching the installed paths.
if [ -n "${GIT_AI_LOCAL_BINARY:-}" ]; then
    echo "Using local git-ai binary (release: ${RELEASE_TAG})..."
    if [ ! -f "$GIT_AI_LOCAL_BINARY" ]; then
        error "Local binary not found at $GIT_AI_LOCAL_BINARY"
    fi
    cp "$GIT_AI_LOCAL_BINARY" "$TMP_FILE"
else
    echo "Downloading git-ai (release: ${RELEASE_TAG})..."
    if ! curl --fail --location --silent --show-error -o "$TMP_FILE" "$DOWNLOAD_URL"; then
        remove_staged_candidate
        error "Failed to download binary (HTTP error)"
    fi
fi

# Basic validation: ensure file is not empty
if [ ! -s "$TMP_FILE" ]; then
    remove_staged_candidate
    error "Downloaded file is empty"
fi

# Verify checksum if embedded (release builds only)
verify_checksum "$TMP_FILE" "$BINARY_NAME"

chmod +x "$TMP_FILE"
if ! CANDIDATE_VERSION_OUTPUT=$("$TMP_FILE" --version 2>&1); then
    error "Downloaded binary failed version validation: $CANDIDATE_VERSION_OUTPUT"
fi
CANDIDATE_VERSION=$(normalize_version "$CANDIDATE_VERSION_OUTPUT")
if [ -z "$CANDIDATE_VERSION" ]; then
    error "Downloaded binary returned an unrecognized version: $CANDIDATE_VERSION_OUTPUT"
fi

EXPECTED_VERSION_SOURCE="${GIT_AI_INSTALL_EXPECTED_VERSION:-}"
if [ -z "$EXPECTED_VERSION_SOURCE" ] \
    && [ "$PINNED_VERSION" != "__VERSION_PLACEHOLDER__" ] \
    && [ "$PINNED_VERSION" != latest ]; then
    EXPECTED_VERSION_SOURCE="$PINNED_VERSION"
fi
EXPECTED_VERSION=""
if [ -n "$EXPECTED_VERSION_SOURCE" ]; then
    EXPECTED_VERSION=$(normalize_version "$EXPECTED_VERSION_SOURCE")
    if [ -z "$EXPECTED_VERSION" ]; then
        error "Expected release version is invalid: $EXPECTED_VERSION_SOURCE"
    fi
    if [ "$CANDIDATE_VERSION" != "$EXPECTED_VERSION" ]; then
        error "Downloaded binary version mismatch: expected $EXPECTED_VERSION, got $CANDIDATE_VERSION"
    fi
fi

# Reject directories at managed executable paths. Moving one as a "backup"
# would be surprising and cannot be restored safely with file-only cleanup.
if [ -d "$FINAL_BINARY" ] && [ ! -L "$FINAL_BINARY" ]; then
    error "Managed binary path is a directory: $FINAL_BINARY"
fi
if [ -d "$GIT_SHIM" ] && [ ! -L "$GIT_SHIM" ]; then
    error "Managed git shim path is a directory: $GIT_SHIM"
fi
if [ -d "$CLI_LINK" ] && [ ! -L "$CLI_LINK" ]; then
    error "Managed CLI link path is a directory: $CLI_LINK"
fi
for backup in "$BINARY_BACKUP" "$GIT_SHIM_BACKUP" "$CLI_LINK_BACKUP"; do
    if path_exists "$backup"; then
        error "Installer backup path already exists: $backup"
    fi
done

BINARY_WAS_PRESENT=false
GIT_SHIM_WAS_PRESENT=false
CLI_LINK_WAS_PRESENT=false
if path_exists "$FINAL_BINARY"; then
    BINARY_WAS_PRESENT=true
    if [ -x "$FINAL_BINARY" ]; then
        PREVIOUS_VERSION_OUTPUT=$("$FINAL_BINARY" --version 2>/dev/null || true)
        PREVIOUS_VERSION=$(normalize_version "$PREVIOUS_VERSION_OUTPUT")
    fi
fi
if path_exists "$GIT_SHIM"; then
    GIT_SHIM_WAS_PRESENT=true
fi
if path_exists "$CLI_LINK"; then
    CLI_LINK_WAS_PRESENT=true
fi

if [ -n "$PREVIOUS_VERSION" ] \
    && version_is_greater "$PREVIOUS_VERSION" "$CANDIDATE_VERSION" \
    && [ "${GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE:-}" != 1 ]; then
    error "Refusing downgrade from $PREVIOUS_VERSION to $CANDIDATE_VERSION because local database schemas are forward-only. Back up ~/.git-ai and validate schema compatibility before retrying with GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE=1."
fi

if ! write_install_journal prepared; then
    error "Could not create the installer recovery journal"
fi

# Preserve all existing executable entry points before publishing any new one,
# so an error can restore a version-consistent set.
if path_exists "$FINAL_BINARY"; then
    mv "$FINAL_BINARY" "$BINARY_BACKUP"
    BINARY_PRESERVED=true
fi
if path_exists "$GIT_SHIM"; then
    mv "$GIT_SHIM" "$GIT_SHIM_BACKUP"
    GIT_SHIM_PRESERVED=true
fi
if path_exists "$CLI_LINK"; then
    mv "$CLI_LINK" "$CLI_LINK_BACKUP"
    CLI_LINK_PRESERVED=true
fi

inject_install_failure_if_requested after_backups_preserved

mv "$TMP_FILE" "$FINAL_BINARY"
BINARY_PUBLISHED=true

# Remove quarantine attribute on macOS
if [ "$OS" = "macos" ]; then
    xattr -d com.apple.quarantine "$FINAL_BINARY" 2>/dev/null || true
fi

inject_install_failure_if_requested after_binary_publish

# Existing wrapper users must see the same version as git-ai. A first install
# does not create the legacy git shim.
if [ "$GIT_SHIM_WAS_PRESENT" = true ]; then
    ln -s "$FINAL_BINARY" "$GIT_SHIM"
    GIT_SHIM_PUBLISHED=true
fi

inject_install_failure_if_requested after_shim_publish

# Create ~/.local/bin/git-ai symlink for systems where ~/.local/bin is already on PATH
if [ ! -d "$LOCAL_ROOT" ]; then
    LOCAL_ROOT_CREATED=true
fi
if [ ! -d "$LOCAL_BIN_DIR" ]; then
    LOCAL_BIN_DIR_CREATED=true
fi
if mkdir -p "$LOCAL_BIN_DIR" 2>/dev/null && ln -s "$FINAL_BINARY" "$CLI_LINK" 2>/dev/null; then
    CLI_LINK_PUBLISHED=true
    success "Created symlink at $CLI_LINK"
elif [ "$CLI_LINK_PRESERVED" = true ]; then
    error "Failed to replace the existing ~/.local/bin/git-ai entry"
else
    warn "Failed to create ~/.local/bin/git-ai symlink. This is non-fatal."
fi

inject_install_failure_if_requested after_cli_link_publish

# Validate the published path again. The exact expected release version is a
# completion gate, not merely informational output.
if ! INSTALLED_VERSION_OUTPUT=$("$FINAL_BINARY" --version 2>&1); then
    error "Installed binary failed version validation: $INSTALLED_VERSION_OUTPUT"
fi
INSTALLED_VERSION=$(normalize_version "$INSTALLED_VERSION_OUTPUT")
if [ -z "$INSTALLED_VERSION" ] || [ "$INSTALLED_VERSION" != "$CANDIDATE_VERSION" ]; then
    error "Published binary version changed during installation: expected $CANDIDATE_VERSION, got ${INSTALLED_VERSION:-unknown}"
fi
if [ -n "$EXPECTED_VERSION" ] && [ "$INSTALLED_VERSION" != "$EXPECTED_VERSION" ]; then
    error "Installed binary version mismatch: expected $EXPECTED_VERSION, got $INSTALLED_VERSION"
fi
echo "Installed git-ai ${INSTALLED_VERSION_OUTPUT}"

# Login user with install token if provided
NEED_LOGIN=false
if [ -n "${INSTALL_NONCE:-}" ] && [ -n "${API_BASE:-}" ]; then
    if ! "$FINAL_BINARY" exchange-nonce; then
        NEED_LOGIN=true
    fi
fi

# Interactive authentication is a required step when nonce exchange failed;
# perform it before non-transactional hook and shell-profile setup.
if [ "$NEED_LOGIN" = true ]; then
    echo ""
    echo "Launching login..."
    if ! "$FINAL_BINARY" login; then
        error "Login failed"
    fi
fi

echo "Setting up IDE/agent hooks..."
# --env also adds git-ai to the PATH in all detected shell configurations;
# GIT_AI_INSTALL_USER lets it hand ownership of any files it creates back to
# the target user in root/MDM installs.
if ! GIT_AI_INSTALL_USER="$INSTALL_USER" "$FINAL_BINARY" install-hooks --env; then
    warn "Warning: Failed to set up IDE/agent hooks. Please try running 'git-ai install-hooks' manually."
else
    success "Successfully set up IDE/agent hooks"
fi

# Fix file ownership when running as root for a different user (MDM deployments)
if [ "$(id -u)" = "0" ] && [ -n "$INSTALL_USER" ]; then
    chown -R "$INSTALL_USER" "$HOME/.git-ai" 2>/dev/null || true
fi

complete_install_transaction

success "Successfully installed git-ai into ${INSTALL_DIR}"
success "You can now run 'git-ai' from your terminal"

echo ""
echo -e "${YELLOW}Close and reopen your terminal and IDE sessions to use git-ai.${NC}"
