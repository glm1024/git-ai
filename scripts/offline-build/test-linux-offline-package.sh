#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "${SCRIPT_DIR}/../.." && pwd)

fail() {
    printf '%s\n' "[linux-offline-test] ERROR: $*" >&2
    exit 1
}

TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/git-ai-linux-offline.XXXXXX")
cleanup_test() {
    rm -rf "${TEST_ROOT}"
}
trap cleanup_test 0 HUP INT TERM

FIXTURE_ROOT="${TEST_ROOT}/repo"
mkdir -p \
    "${FIXTURE_ROOT}/scripts/offline-build" \
    "${FIXTURE_ROOT}/build/offline-build/artifacts/linux" \
    "${FIXTURE_ROOT}/offline-dist"

cp "${REPO_ROOT}/Cargo.toml" "${FIXTURE_ROOT}/Cargo.toml"
cp "${REPO_ROOT}/install.sh" "${FIXTURE_ROOT}/install.sh"
cp "${SCRIPT_DIR}/common.sh" "${FIXTURE_ROOT}/scripts/offline-build/common.sh"
cp "${SCRIPT_DIR}/package-linux.sh" "${FIXTURE_ROOT}/scripts/offline-build/package-linux.sh"
cp "${SCRIPT_DIR}/LINUX-INSTALL.md" "${FIXTURE_ROOT}/scripts/offline-build/LINUX-INSTALL.md"

awk '
    !updated && /^version = / {
        print "version = \"9.8.7\""
        updated = 1
        next
    }
    { print }
' "${FIXTURE_ROOT}/Cargo.toml" > "${FIXTURE_ROOT}/Cargo.toml.tmp"
mv "${FIXTURE_ROOT}/Cargo.toml.tmp" "${FIXTURE_ROOT}/Cargo.toml"

{
    printf '%s\n' '/build/'
    printf '%s\n' '/offline-dist/'
} > "${FIXTURE_ROOT}/.gitignore"

git -C "${FIXTURE_ROOT}" init -q
git -C "${FIXTURE_ROOT}" add .
git -C "${FIXTURE_ROOT}" \
    -c user.name='Git AI Test' \
    -c user.email='git-ai-test@example.invalid' \
    commit -qm 'fixture'
SOURCE_COMMIT=$(git -C "${FIXTURE_ROOT}" rev-parse HEAD)

write_fake_binary() {
    artifact=$1
    selected_arch=$2
    cat > "${artifact}" <<EOF
#!/bin/sh
case "\${1:-}" in
    --version)
        printf '%s\n' 'git-ai 9.8.7'
        printf '%s\n' '${selected_arch}' > "\${HOME}/selected-architecture"
        ;;
    install-hooks|exchange-nonce|login|bg) exit 0 ;;
esac
exit 0
EOF
    chmod 755 "${artifact}"
}

write_artifact_metadata() {
    artifact=$1
    artifact_sha=$(shasum -a 256 "${artifact}" | awk '{print $1}')
    {
        printf '%s\n' 'format=git-ai-offline-artifact-v1'
        printf 'artifact_name=%s\n' "$(basename "${artifact}")"
        printf 'artifact_sha256=%s\n' "${artifact_sha}"
        printf 'source_commit=%s\n' "${SOURCE_COMMIT}"
        printf '%s\n' 'source_dirty=false'
        printf '%s\n' 'built_at_utc=2026-01-01T00:00:00Z'
    } > "${artifact}.build-metadata"
}

X64_ARTIFACT="${FIXTURE_ROOT}/build/offline-build/artifacts/linux/git-ai-linux-x64"
ARM64_ARTIFACT="${FIXTURE_ROOT}/build/offline-build/artifacts/linux/git-ai-linux-arm64"
write_fake_binary "${X64_ARTIFACT}" x64
write_fake_binary "${ARM64_ARTIFACT}" arm64
write_artifact_metadata "${X64_ARTIFACT}"
write_artifact_metadata "${ARM64_ARTIFACT}"

sh "${FIXTURE_ROOT}/scripts/offline-build/package-linux.sh" >/dev/null

ARCHIVE="${FIXTURE_ROOT}/offline-dist/git-ai-linux-v9.8.7.tar.gz"
[ -f "${ARCHIVE}" ] || fail "Linux offline archive was not created"
ARCHIVE_LIST="${TEST_ROOT}/archive-list.txt"
tar -tzf "${ARCHIVE}" > "${ARCHIVE_LIST}"
for required in \
    git-ai-linux-v9.8.7/install.sh \
    git-ai-linux-v9.8.7/INSTALL.md \
    git-ai-linux-v9.8.7/BUILD-METADATA.txt \
    git-ai-linux-v9.8.7/SHA256SUMS \
    git-ai-linux-v9.8.7/linux/git-ai-linux-x64 \
    git-ai-linux-v9.8.7/linux/git-ai-linux-arm64
do
    grep -Fxq "${required}" "${ARCHIVE_LIST}" \
        || fail "Archive is missing ${required}"
done

EXTRACT_ROOT="${TEST_ROOT}/extracted"
mkdir -p "${EXTRACT_ROOT}"
tar -xzf "${ARCHIVE}" -C "${EXTRACT_ROOT}"
PACKAGE_DIR="${EXTRACT_ROOT}/git-ai-linux-v9.8.7"

grep -Fq 'BUNDLED_BINARY_DIR="linux"' "${PACKAGE_DIR}/install.sh" \
    || fail "Packaged installer does not enable bundled Linux binary selection"
grep -Fq 'BUNDLED_TARGET_OS="linux"' "${PACKAGE_DIR}/install.sh" \
    || fail "Packaged installer does not restrict itself to Linux"
if grep -Fq 'EMBEDDED_CHECKSUMS="__CHECKSUMS_PLACEHOLDER__"' "${PACKAGE_DIR}/install.sh"; then
    fail "Packaged installer does not contain internal binary checksums"
fi

FAKE_BIN="${TEST_ROOT}/fake-bin"
mkdir -p "${FAKE_BIN}"
cat > "${FAKE_BIN}/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) printf '%s\n' Linux ;;
    -m) printf '%s\n' "${GIT_AI_TEST_UNAME_M:-x86_64}" ;;
    -a) printf '%s\n' "Linux fixture ${GIT_AI_TEST_UNAME_M:-x86_64}" ;;
    *) exit 2 ;;
esac
EOF
chmod 755 "${FAKE_BIN}/uname"

run_installer_for_arch() {
    uname_arch=$1
    expected_arch=$2
    home_dir="${TEST_ROOT}/home-${uname_arch}"
    mkdir -p "${home_dir}"

    HOME="${home_dir}" \
    SHELL=/bin/bash \
    PATH="${FAKE_BIN}:${PATH}" \
    GIT_AI_TEST_UNAME_M="${uname_arch}" \
    GIT_AI_ALLOW_SUPERUSER=1 \
        bash "${PACKAGE_DIR}/install.sh" >/dev/null 2>&1

    [ "$(cat "${home_dir}/selected-architecture")" = "${expected_arch}" ] \
        || fail "${uname_arch} selected the wrong bundled binary"
    [ "$("${home_dir}/.git-ai/bin/git-ai" --version | head -n 1)" = 'git-ai 9.8.7' ] \
        || fail "${uname_arch} did not install the bundled CLI"
}

run_installer_for_arch x86_64 x64
run_installer_for_arch aarch64 arm64
run_installer_for_arch arm64 arm64

UNSUPPORTED_HOME="${TEST_ROOT}/home-unsupported"
mkdir -p "${UNSUPPORTED_HOME}"
if HOME="${UNSUPPORTED_HOME}" \
    SHELL=/bin/bash \
    PATH="${FAKE_BIN}:${PATH}" \
    GIT_AI_TEST_UNAME_M=ppc64le \
    GIT_AI_ALLOW_SUPERUSER=1 \
        bash "${PACKAGE_DIR}/install.sh" >/dev/null 2>&1
then
    fail "Unsupported Linux architecture unexpectedly installed"
fi
[ ! -e "${UNSUPPORTED_HOME}/.git-ai/bin/git-ai" ] \
    || fail "Unsupported architecture changed the installation"

printf '%s\n' 'tampered' >> "${PACKAGE_DIR}/linux/git-ai-linux-x64"
TAMPERED_HOME="${TEST_ROOT}/home-tampered"
mkdir -p "${TAMPERED_HOME}"
if HOME="${TAMPERED_HOME}" \
    SHELL=/bin/bash \
    PATH="${FAKE_BIN}:${PATH}" \
    GIT_AI_TEST_UNAME_M=x86_64 \
    GIT_AI_ALLOW_SUPERUSER=1 \
        bash "${PACKAGE_DIR}/install.sh" >/dev/null 2>&1
then
    fail "Tampered bundled binary unexpectedly installed"
fi
[ ! -e "${TAMPERED_HOME}/.git-ai/bin/git-ai" ] \
    || fail "Checksum rejection changed the installation"

printf '%s\n' '[linux-offline-test] Linux packaging, architecture selection, and internal checksum tests passed'
