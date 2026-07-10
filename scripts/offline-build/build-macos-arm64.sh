#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"

if [ "${1:-}" = "--help" ]; then
    cat <<'EOF'
Build the native macOS Apple Silicon CLI binary.

Requirements:
  - macOS on Apple Silicon with Xcode Command Line Tools.
  - rustup with Rust 1.93.0, or GIT_AI_MACOS_RUST_TOOLCHAIN set to a
    compatible installed Rust toolchain.

Environment:
  GIT_AI_BUILD_OFFLINE=1          Require the Rust toolchain and Cargo cache.
  GIT_AI_BUILD_ROOT=PATH          Override build/offline-build.
  GIT_AI_MACOS_RUST_TOOLCHAIN=X   Override the default Rust 1.93.0 toolchain.
EOF
    exit 0
fi

require_command rustup
require_command strip
require_command file
prepare_build_dirs

if [ "$(uname -s)" != "Darwin" ]; then
    fail "macOS ARM64 builds must run on a native Apple Silicon macOS host."
fi

case "$(uname -m)" in
    arm64|aarch64) ;;
    *) fail "This script requires an Apple Silicon host; found $(uname -m)." ;;
esac

TARGET=aarch64-apple-darwin
ARTIFACT=git-ai-macos-arm64
TOOLCHAIN=${GIT_AI_MACOS_RUST_TOOLCHAIN:-${RUST_VERSION}}
OUT_DIR="${ARTIFACT_ROOT}/macos"
TARGET_DIR="${BUILD_ROOT}/target/macos-arm64"
SOURCE_BINARY="${TARGET_DIR}/${TARGET}/release/git-ai"

mkdir -p "${OUT_DIR}" "${TARGET_DIR}"

if ! rustup toolchain list | awk -v toolchain="${TOOLCHAIN}" '$1 == toolchain || index($1, toolchain "-") == 1 { found = 1 } END { exit !found }'; then
    if is_offline_build; then
        fail "Rust toolchain ${TOOLCHAIN} is missing in offline mode."
    fi
    info "Installing Rust toolchain ${TOOLCHAIN}"
    rustup toolchain install "${TOOLCHAIN}" --profile minimal
fi

if ! rustup target list --toolchain "${TOOLCHAIN}" --installed | grep -qx "${TARGET}"; then
    if is_offline_build; then
        fail "Rust target ${TARGET} is missing in offline mode."
    fi
    info "Installing Rust target ${TARGET} for ${TOOLCHAIN}"
    rustup target add --toolchain "${TOOLCHAIN}" "${TARGET}"
fi

CARGO_BIN=$(rustup which --toolchain "${TOOLCHAIN}" cargo)
RUSTC_BIN=$(rustup which --toolchain "${TOOLCHAIN}" rustc)
RUSTDOC_BIN=$(rustup which --toolchain "${TOOLCHAIN}" rustdoc)

info "Building ${ARTIFACT} with Rust ${TOOLCHAIN}"
if is_offline_build; then
    (
        cd "${REPO_ROOT}"
        CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" OSS_BUILD=1 \
            RUSTC="${RUSTC_BIN}" RUSTDOC="${RUSTDOC_BIN}" \
            "${CARGO_BIN}" build --offline --release --target "${TARGET}"
    )
else
    (
        cd "${REPO_ROOT}"
        CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="${TARGET_DIR}" OSS_BUILD=1 \
            RUSTC="${RUSTC_BIN}" RUSTDOC="${RUSTDOC_BIN}" \
            "${CARGO_BIN}" build --release --target "${TARGET}"
    )
fi

require_file "${SOURCE_BINARY}"
strip "${SOURCE_BINARY}"
cp "${SOURCE_BINARY}" "${OUT_DIR}/${ARTIFACT}"
file "${OUT_DIR}/${ARTIFACT}"
