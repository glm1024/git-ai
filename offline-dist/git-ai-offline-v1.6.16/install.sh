#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

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
REPO="internal/git-ai-offline"
if [ "$REPO" = "__REPO_PLACEHOLDER__" ]; then
    REPO="git-ai-project/git-ai"
fi

# Version placeholder - replaced during release builds with actual version (e.g., "v1.0.24")
# When set to __VERSION_PLACEHOLDER__, defaults to "latest"
PINNED_VERSION="v1.6.16"

# Embedded checksums - replaced during release builds with actual SHA256 checksums
# Format: "hash  filename|hash  filename|..." (pipe-separated)
# When set to __CHECKSUMS_PLACEHOLDER__, checksum verification is skipped
EMBEDDED_CHECKSUMS="ea2b3431d73e38b26f05efe88b4822434305e47cc5a674873d65760e9711ef2f  git-ai-linux-arm64|b35b828932e2fb024ca267643744b95bcfefa6b1522ce5a3b27ef93447882123  git-ai-linux-x64|05e924ad48698ac250113ccef6fd72a89d5bd370bfcdf88a2742baca5215a4eb  git-ai-macos-arm64|281ba9afdc867b236974947c4dbb8e6ad5fca368d4df3f7ac36b31b299768aa8  git-ai-windows-x64.exe"

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
        warn "Neither sha256sum nor shasum available, skipping checksum verification"
        return 0
    fi

    if [ "$expected" != "$actual" ]; then
        rm -f "$file" 2>/dev/null || true
        error "Checksum verification failed for $binary_name\nExpected: $expected\nActual:   $actual"
    fi

    success "Checksum verified for $binary_name"
}

# Function to detect all shells with existing config files
# Returns shell configurations in format: "shell_name|config_file" (one per line)
detect_all_shells() {
    local shells=""
    
    # Check for bash configs (prefer .bashrc over .bash_profile)
    if [ -f "$HOME/.bashrc" ]; then
        shells="${shells}bash|$HOME/.bashrc\n"
    elif [ -f "$HOME/.bash_profile" ]; then
        shells="${shells}bash|$HOME/.bash_profile\n"
    fi
    
    # Check for zsh config
    if [ -f "$HOME/.zshrc" ]; then
        shells="${shells}zsh|$HOME/.zshrc\n"
    fi
    
    # Check for fish config
    if [ -f "$HOME/.config/fish/config.fish" ]; then
        shells="${shells}fish|$HOME/.config/fish/config.fish\n"
    fi
    
    # If no configs found, fall back to $SHELL detection and create config for that shell only
    if [ -z "$shells" ]; then
        local login_shell=""
        if [ -n "${SHELL:-}" ]; then
            login_shell=$(basename "$SHELL")
        fi
        case "$login_shell" in
            fish)
                shells="fish|$HOME/.config/fish/config.fish"
                ;;
            zsh)
                shells="zsh|$HOME/.zshrc"
                ;;
            bash|*)
                shells="bash|$HOME/.bashrc"
                ;;
        esac
    fi
    
    # Remove trailing newline and output
    printf '%b' "$shells" | sed '/^$/d'
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

# Install into the user's bin directory ~/.git-ai/bin. Binary and shim
# publication is transactional: an upgrade keeps the previous paths until the
# new binary has been validated, and a failed first install removes only paths
# created by this installer (never ~/.git-ai data/configuration).
INSTALL_ROOT="$HOME/.git-ai"
INSTALL_DIR="${INSTALL_ROOT}/bin"
FINAL_BINARY="${INSTALL_DIR}/git-ai"
GIT_SHIM="${INSTALL_DIR}/git"
LOCAL_ROOT="$HOME/.local"
LOCAL_BIN_DIR="${LOCAL_ROOT}/bin"
CLI_LINK="${LOCAL_BIN_DIR}/git-ai"
TMP_FILE="${INSTALL_DIR}/git-ai.tmp.$$"
BINARY_BACKUP="${FINAL_BINARY}.install-backup.$$"
GIT_SHIM_BACKUP="${GIT_SHIM}.install-backup.$$"
CLI_LINK_BACKUP="${CLI_LINK}.install-backup.$$"

INSTALL_ROOT_CREATED=false
INSTALL_DIR_CREATED=false
LOCAL_ROOT_CREATED=false
LOCAL_BIN_DIR_CREATED=false
BINARY_PRESERVED=false
GIT_SHIM_PRESERVED=false
CLI_LINK_PRESERVED=false
BINARY_PUBLISHED=false
GIT_SHIM_PUBLISHED=false
CLI_LINK_PUBLISHED=false
GIT_SHIM_WAS_PRESENT=false
INSTALL_TRANSACTION_ACTIVE=false

path_exists() {
    [ -e "$1" ] || [ -L "$1" ]
}

rollback_install_transaction() {
    local original_status="${1:-1}"
    local restore_failed=false

    trap - EXIT HUP INT TERM
    set +e

    if [ "$INSTALL_TRANSACTION_ACTIVE" != true ]; then
        exit "$original_status"
    fi

    [ "$original_status" -ne 0 ] || original_status=1

    if path_exists "$TMP_FILE" && ! rm -f "$TMP_FILE" 2>/dev/null; then
        restore_failed=true
        warn "Failed to remove staged binary: $TMP_FILE"
    fi

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

    if [ "$restore_failed" = true ]; then
        warn "Installation failed and at least one previous executable path needs manual recovery. Preserved data under $INSTALL_ROOT was not removed."
    fi
    exit "$original_status"
}

complete_install_transaction() {
    INSTALL_TRANSACTION_ACTIVE=false
    trap - EXIT HUP INT TERM

    for backup in "$BINARY_BACKUP" "$GIT_SHIM_BACKUP" "$CLI_LINK_BACKUP"; do
        if path_exists "$backup" && ! rm -f "$backup"; then
            warn "Installed successfully, but could not remove obsolete backup: $backup"
        fi
    done
    rm -f "$TMP_FILE" 2>/dev/null || true
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
INSTALL_TRANSACTION_ACTIVE=true
trap 'rollback_install_transaction $?' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$INSTALL_DIR"

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
        rm -f "$TMP_FILE" 2>/dev/null || true
        error "Failed to download binary (HTTP error)"
    fi
fi

# Basic validation: ensure file is not empty
if [ ! -s "$TMP_FILE" ]; then
    rm -f "$TMP_FILE" 2>/dev/null || true
    error "Downloaded file is empty"
fi

# Verify checksum if embedded (release builds only)
verify_checksum "$TMP_FILE" "$BINARY_NAME"

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

# Preserve all existing executable entry points before publishing any new one,
# so an error can restore a version-consistent set.
if path_exists "$FINAL_BINARY"; then
    mv "$FINAL_BINARY" "$BINARY_BACKUP"
    BINARY_PRESERVED=true
fi
if path_exists "$GIT_SHIM"; then
    GIT_SHIM_WAS_PRESENT=true
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

# Make executable
chmod +x "$FINAL_BINARY"

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

# Print installed version
if ! INSTALLED_VERSION=$("$FINAL_BINARY" --version 2>&1); then
    error "Installed binary failed version validation: $INSTALLED_VERSION"
fi
echo "Installed git-ai ${INSTALLED_VERSION}"

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
if ! "$FINAL_BINARY" install-hooks; then
    warn "Warning: Failed to set up IDE/agent hooks. Please try running 'git-ai install-hooks' manually."
else
    success "Successfully set up IDE/agent hooks"
fi

# Add to PATH in all detected shell configurations
SHELLS_CONFIGURED=""
SHELLS_ALREADY_CONFIGURED=""
CREATED_SHELL_PATHS=""

while IFS='|' read -r shell_name config_file; do
    [ -z "$shell_name" ] && continue
    
    # Generate shell-appropriate PATH command
    if [ "$shell_name" = "fish" ]; then
        path_cmd="fish_add_path -g \"$INSTALL_DIR\""
        # Create fish config directory if it doesn't exist (for fallback case)
        config_dir="$(dirname "$config_file")"
        if [ ! -d "$config_dir" ]; then
            if ! mkdir -p "$config_dir"; then
                warn "Failed to create shell configuration directory: $config_dir"
                continue
            fi
            CREATED_SHELL_PATHS="${CREATED_SHELL_PATHS}${config_dir}\n"
        fi
    else
        path_cmd="export PATH=\"$INSTALL_DIR:\$PATH\""
    fi
    
    # Create config file if it doesn't exist (for fallback case when no configs found)
    if [ ! -f "$config_file" ]; then
        CREATED_SHELL_PATHS="${CREATED_SHELL_PATHS}${config_file}\n"
    fi
    if ! touch "$config_file"; then
        warn "Failed to create shell configuration: $config_file"
        continue
    fi
    
    # Append if not already present
    if ! grep -qsF "$INSTALL_DIR" "$config_file"; then
        if {
            echo ""
            echo "# Added by git-ai installer on $(date)"
            echo "$path_cmd"
        } >> "$config_file"; then
            SHELLS_CONFIGURED="${SHELLS_CONFIGURED}${shell_name}|${config_file}\n"
        else
            warn "Failed to update shell configuration: $config_file"
        fi
    else
        SHELLS_ALREADY_CONFIGURED="${SHELLS_ALREADY_CONFIGURED}${shell_name}|${config_file}\n"
    fi
done <<< "$(detect_all_shells)"

# Display results to user
if [ -n "$SHELLS_CONFIGURED" ]; then
    echo ""
    echo "Updated shell configurations:"
    printf '%b' "$SHELLS_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        success "  ✓ $config_file"
    done
    
    echo ""
    echo "To apply changes immediately:"
    printf '%b' "$SHELLS_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        if [ "$shell_name" = "fish" ]; then
            echo "  - For fish: source $config_file"
        else
            echo "  - For $shell_name: source $config_file"
        fi
    done
fi

if [ -n "$SHELLS_ALREADY_CONFIGURED" ]; then
    echo ""
    echo "Already configured (no changes needed):"
    printf '%b' "$SHELLS_ALREADY_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        echo "  ✓ $config_file"
    done
fi

if [ -z "$SHELLS_CONFIGURED" ] && [ -z "$SHELLS_ALREADY_CONFIGURED" ]; then
    echo ""
    echo "Could not detect any shell config files."
    echo "Please add the following line to your shell config and restart:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi

# Fix file ownership when running as root for a different user (MDM deployments)
if [ "$(id -u)" = "0" ] && [ -n "$INSTALL_USER" ]; then
    chown -R "$INSTALL_USER" "$HOME/.git-ai" 2>/dev/null || true
    if [ -n "$CREATED_SHELL_PATHS" ]; then
        printf '%b' "$CREATED_SHELL_PATHS" | while IFS= read -r created_path; do
            [ -z "$created_path" ] && continue
            chown "$INSTALL_USER" "$created_path" 2>/dev/null || true
        done
    fi
fi

complete_install_transaction

success "Successfully installed git-ai into ${INSTALL_DIR}"
success "You can now run 'git-ai' from your terminal"

echo ""
echo -e "${YELLOW}Close and reopen your terminal and IDE sessions to use git-ai.${NC}"
