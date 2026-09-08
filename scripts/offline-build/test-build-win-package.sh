#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/git-ai-build-win.XXXXXX")

cleanup() {
    rm -rf "${TEST_ROOT}"
}
trap cleanup 0 HUP INT TERM

mkdir -p "${TEST_ROOT}/repo/scripts/offline-build"
cp "${SCRIPT_DIR}/build-win.sh" "${TEST_ROOT}/repo/scripts/offline-build/"
cp "${SCRIPT_DIR}/common.sh" "${TEST_ROOT}/repo/scripts/offline-build/"
cp "${SCRIPT_DIR}/WINDOWS-INSTALL.md" "${TEST_ROOT}/repo/scripts/offline-build/"
cat > "${TEST_ROOT}/repo/Cargo.toml" <<'EOF'
[package]
name = "git-ai-package-test"
version = "9.8.7"
EOF
cat > "${TEST_ROOT}/repo/install.ps1" <<'EOF'
Write-Output 'test installer'
EOF
cat > "${TEST_ROOT}/repo/.gitignore" <<'EOF'
/build/
/offline-dist/.git-ai-windows-package-*/
/offline-dist/.git-ai-windows-package-*.zip
/offline-dist/git-ai-windows-v*.zip
EOF
cat > "${TEST_ROOT}/repo/scripts/offline-build/build-windows-x64.sh" <<'EOF'
#!/bin/sh
set -eu
SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
. "${SCRIPT_DIR}/common.sh"
prepare_build_dirs
artifact="${ARTIFACT_ROOT}/windows/git-ai-windows-x64.exe"
mkdir -p "$(dirname "${artifact}")"
begin_artifact_build "${artifact}"
printf 'synthetic Windows executable\n' > "${artifact}"
finish_artifact_build "${artifact}"
EOF
chmod +x "${TEST_ROOT}/repo/scripts/offline-build/build-windows-x64.sh"

git -C "${TEST_ROOT}/repo" init -q
git -C "${TEST_ROOT}/repo" config user.name test
git -C "${TEST_ROOT}/repo" config user.email test@example.invalid
git -C "${TEST_ROOT}/repo" add .
git -C "${TEST_ROOT}/repo" commit -qm fixture

sh "${TEST_ROOT}/repo/scripts/offline-build/build-win.sh" >/dev/null
archive="${TEST_ROOT}/repo/offline-dist/git-ai-windows-v9.8.7.zip"
[ -f "${archive}" ] || {
    printf '%s\n' '[build-win-test] ERROR: final ZIP was not created' >&2
    exit 1
}
[ "$(find "${TEST_ROOT}/repo/offline-dist" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')" = 1 ] || {
    printf '%s\n' '[build-win-test] ERROR: output directory contains more than the final ZIP' >&2
    exit 1
}
unzip -tq "${archive}" >/dev/null
unzip -q "${archive}" -d "${TEST_ROOT}/extracted"
(
    cd "${TEST_ROOT}/extracted/git-ai-windows-v9.8.7"
    shasum -a 256 -c SHA256SUMS >/dev/null
)

printf '%s\n' '[build-win-test] Windows ZIP packaging passed'
