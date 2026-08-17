#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "${SCRIPT_DIR}/../.." && pwd)

fail() {
    printf '%s\n' "[offline-release-contract] ERROR: $*" >&2
    exit 1
}

assert_contains() {
    file=$1
    value=$2
    grep -Fq "${value}" "${file}" || fail "$(basename "${file}") is missing: ${value}"
}

sh -n "${SCRIPT_DIR}/package-offline-dist.sh"
if command -v dash >/dev/null 2>&1; then
    dash -n "${SCRIPT_DIR}/package-offline-dist.sh"
fi
bash -n "${REPO_ROOT}/install.sh"
help_home="$(mktemp -d "${TMPDIR:-/tmp}/git-ai-install-help.XXXXXX")"
HOME="${help_home}" sh "${REPO_ROOT}/install.sh" --help >/dev/null
[ ! -e "${help_home}/.git-ai" ] || fail 'sh install.sh --help must not create install state'
rm -rf "${help_home}"

cli_version=$(awk -F '"' '/^version = "/ { print $2; exit }' "${REPO_ROOT}/Cargo.toml")
lock_version=$(awk '
    /^name = "git-ai"$/ { in_package = 1; next }
    in_package && /^version = "/ { gsub(/^version = "|"$/, ""); print; exit }
' "${REPO_ROOT}/Cargo.lock")
flake_version=$(awk -F '"' '/^[[:space:]]*version = "/ { print $2; exit }' "${REPO_ROOT}/flake.nix")
[ "${cli_version}" = 1.6.17 ] || fail "CLI release version must be 1.6.17, got ${cli_version}"
[ "${lock_version}" = "${cli_version}" ] || fail "Cargo.lock version does not match Cargo.toml"
[ "${flake_version}" = "${cli_version}" ] || fail "flake.nix version does not match Cargo.toml"

assert_contains "${REPO_ROOT}/agent-support/vscode/package.json" '"version": "0.1.24"'
assert_contains "${REPO_ROOT}/agent-support/vscode/package-lock.json" '"version": "0.1.24"'
assert_contains "${REPO_ROOT}/agent-support/vscode/CHANGELOG.md" '## [0.1.24]'

package_script="${SCRIPT_DIR}/package-offline-dist.sh"
assert_contains "${package_script}" '[ "${OFFLINE_VERSION}" = "${CLI_VERSION}" ]'
assert_contains "${package_script}" 'INSTALL.template.md'
assert_contains "${package_script}" 'metrics_schema_version=%s'
assert_contains "${package_script}" 'schema_compatibility=forward-only'
assert_contains "${package_script}" 'INSTALL.md \'
assert_contains "${package_script}" 'BUILD-METADATA.txt'
assert_contains "${package_script}" 'GIT_AI_PACKAGED_DIST_UNDER_TEST="${STAGING_DIR}"'
if grep -Fq 'offline-dist/git-ai-offline-v1.6.16' "${SCRIPT_DIR}/test-install-rollback.sh"; then
    fail 'Source installer regression must not force parity with historical v1.6.16 artifacts'
fi

metadata_line=$(grep -n -F '} > "${STAGING_DIR}/BUILD-METADATA.txt"' "${package_script}" | cut -d: -f1)
checksums_line=$(grep -n -F 'RAW_SHA256SUMS=' "${package_script}" | cut -d: -f1)
[ -n "${metadata_line}" ] && [ -n "${checksums_line}" ] && [ "${metadata_line}" -lt "${checksums_line}" ] \
    || fail 'BUILD-METADATA.txt must be written before SHA256SUMS is generated'

template="${SCRIPT_DIR}/INSTALL.template.md"
for token in __OFFLINE_VERSION__ __VSCODE_VSIX__ __JETBRAINS_ZIP__; do
    assert_contains "${template}" "${token}"
done
assert_contains "${template}" 'git-ai config reporting-profile set --stdin'
profile_line=$(grep -n -F 'git-ai config reporting-profile set --stdin' "${template}" | head -n 1 | cut -d: -f1)
daemon_line=$(grep -n -F 'git-ai bg start' "${template}" | head -n 1 | cut -d: -f1)
[ -n "${profile_line}" ] && [ -n "${daemon_line}" ] && [ "${profile_line}" -lt "${daemon_line}" ] \
    || fail 'CLI-only instructions must configure reporting-profile before starting the daemon'

test_root=$(mktemp -d "${TMPDIR:-/tmp}/git-ai-release-contract.XXXXXX")
cleanup_test() {
    rm -rf "${test_root}"
}
trap cleanup_test 0 HUP INT TERM
rendered="${test_root}/INSTALL.md"
awk -v version="${cli_version}" \
    -v vsix='git-ai.git-ai-vscode-0.1.24.vsix' \
    -v jetbrains='Git_AI-0.1.13.zip' '
    {
        gsub(/__OFFLINE_VERSION__/, version)
        gsub(/__VSCODE_VSIX__/, vsix)
        gsub(/__JETBRAINS_ZIP__/, jetbrains)
        print
    }
' "${template}" > "${rendered}"
if grep -Eq '__[A-Z0-9_]+__' "${rendered}"; then
    fail 'Rendered INSTALL.md retained a template token'
fi

assert_contains "${REPO_ROOT}/install.sh" 'neither sha256sum nor shasum is available'
if grep -Fq 'skipping checksum verification' "${REPO_ROOT}/install.sh"; then
    fail 'Unix installer still permits checksum-tool absence to bypass verification'
fi
for marker in \
    'install-transaction' \
    'GIT_AI_INSTALL_EXPECTED_VERSION' \
    'GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE'
do
    assert_contains "${REPO_ROOT}/install.sh" "${marker}"
done

for marker in \
    'install-transaction.json' \
    'GIT_AI_UPDATE_RECEIPT_PATH' \
    'Write-UpgradeReceiptIfRequested' \
    'GIT_AI_INSTALL_EXPECTED_VERSION' \
    'GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE' \
    'MoveFileExW' \
    'MOVEFILE_WRITE_THROUGH' \
    "Join-Path \$stagingDir 'git-ai.exe'" \
    'Initialize-StagingDirectory' \
    'after_committed_journal_before_receipt' \
    'Complete-RecoveredUpgradeReceipt' \
    "@('-h', '--help')"
do
    assert_contains "${REPO_ROOT}/install.ps1" "${marker}"
done
complete_line=$(grep -n -F 'function Complete-InstallTransaction {' "${REPO_ROOT}/install.ps1" | cut -d: -f1)
receipt_line=$(grep -n -F 'Write-UpgradeReceiptIfRequested -InstalledVersion $InstalledVersion' "${REPO_ROOT}/install.ps1" | cut -d: -f1)
unlock_line=$(awk -v start="${complete_line}" 'NR > start && /Exit-InstallLock/ { print NR; exit }' "${REPO_ROOT}/install.ps1")
[ -n "${receipt_line}" ] && [ -n "${unlock_line}" ] && [ "${receipt_line}" -lt "${unlock_line}" ] \
    || fail 'Windows upgrade receipt must be durably published before releasing the installer lock'

upgrade_source="${REPO_ROOT}/src/commands/upgrade.rs"
for marker in \
    'daemon_upgrade_scheduled' \
    'upgrade_scheduled' \
    'validate_upgrade_receipt' \
    'clear_pending_update_cache_strict(channel)' \
    'remove_upgrade_receipt(&path)' \
    'receipt_cleanup_completed' \
    'UpgradeReceiptFileResult::CleanupOnly'
do
    assert_contains "${upgrade_source}" "${marker}"
done
assert_contains "${upgrade_source}" 'pin_pending_release_for_install_strict(channel, &release)'
assert_contains "${upgrade_source}" 'update_pending_receipt_recovery_ready'
clear_line=$(grep -n -F 'clear_pending_update_cache_strict(channel)' "${upgrade_source}" | head -n 1 | cut -d: -f1)
remove_line=$(grep -n -F 'let cleanup_error = remove_upgrade_receipt(&path)' "${upgrade_source}" | head -n 1 | cut -d: -f1)
[ -n "${clear_line}" ] && [ -n "${remove_line}" ] && [ "${clear_line}" -lt "${remove_line}" ] \
    || fail 'Windows completion must durably clear pending cache before consuming its receipt'

printf '%s\n' '[offline-release-contract] Static release-contract tests passed'
