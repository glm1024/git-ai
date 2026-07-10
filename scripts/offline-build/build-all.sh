#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build every supported offline artifact and package a new offline bundle.

This runs, in order:
  macOS Apple Silicon CLI, Linux ARM64 CLI, Linux x64 CLI, Windows x64 CLI,
  VS Code/Cursor VSIX, JetBrains ZIP, then package-offline-dist.sh.

Use the individual scripts in this directory to rebuild only one artifact.
EOF
    exit 0
fi

for build_script in \
    build-macos-arm64.sh \
    build-linux-arm64.sh \
    build-linux-x64.sh \
    build-windows-x64.sh \
    build-vscode.sh \
    build-jetbrains.sh \
    package-offline-dist.sh
do
    sh "${SCRIPT_DIR}/${build_script}"
done
