#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build and package the Linux x64 and ARM64 CLIs for offline installation.

The final deliverable is one archive containing both architectures:
  offline-dist/git-ai-linux-v<CLI version>.tar.gz
EOF
    exit 0
fi

require_clean_release_source
sh "${SCRIPT_DIR}/build-linux-x64.sh"
sh "${SCRIPT_DIR}/build-linux-arm64.sh"
sh "${SCRIPT_DIR}/package-linux.sh"
