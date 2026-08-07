#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "${SCRIPT_DIR}/../.." && pwd)

fail() {
    printf '%s\n' "[offline-install-test] ERROR: $*" >&2
    exit 1
}

TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/git-ai-install-rollback.XXXXXX")
cleanup_test() {
    rm -rf "${TEST_ROOT}"
}
trap cleanup_test 0 HUP INT TERM

FAKE_NEW_BINARY="${TEST_ROOT}/git-ai-new"
cat > "${FAKE_NEW_BINARY}" <<'EOF'
#!/bin/sh
case "${1:-}" in
    --version) printf '%s\n' 'git-ai test-new' ;;
    exchange-nonce|install-hooks|login) exit 0 ;;
    bg) exit 0 ;;
esac
exit 0
EOF
chmod 755 "${FAKE_NEW_BINARY}"

prepare_installer() {
    source_installer=$1
    prepared_installer=$2
    awk '
        /^EMBEDDED_CHECKSUMS=/ {
            print "EMBEDDED_CHECKSUMS=\"__CHECKSUMS_PLACEHOLDER__\""
            next
        }
        { print }
    ' "${source_installer}" > "${prepared_installer}"
    chmod 755 "${prepared_installer}"
}

assert_no_temporary_install_files() {
    home_dir=$1
    leftovers=$(find "${home_dir}" -name '*.install-backup.*' -o -name 'git-ai.tmp.*' 2>/dev/null || true)
    [ -z "${leftovers}" ] || fail "Temporary install files were left behind: ${leftovers}"
}

write_old_install() {
    home_dir=$1
    mkdir -p "${home_dir}/.git-ai/bin" "${home_dir}/.local/bin"
    printf '%s\n' 'old binary bytes' > "${home_dir}/.git-ai/bin/git-ai"
    printf '%s\n' 'old git shim bytes' > "${home_dir}/.git-ai/bin/git"
    printf '%s\n' 'old cli link bytes' > "${home_dir}/.local/bin/git-ai"
    chmod 755 \
        "${home_dir}/.git-ai/bin/git-ai" \
        "${home_dir}/.git-ai/bin/git" \
        "${home_dir}/.local/bin/git-ai"
}

run_installer() {
    installer=$1
    home_dir=$2
    fail_at=${3:-}
    HOME="${home_dir}" \
    SHELL=/bin/bash \
    GIT_AI_ALLOW_SUPERUSER=1 \
    GIT_AI_LOCAL_BINARY="${FAKE_NEW_BINARY}" \
    GIT_AI_INSTALL_TEST_FAIL_AT="${fail_at}" \
        bash "${installer}" >/dev/null 2>&1
}

test_upgrade_rollback() {
    installer=$1
    label=$2
    fail_at=$3
    home_dir="${TEST_ROOT}/${label}-${fail_at}"
    mkdir -p "${home_dir}"
    write_old_install "${home_dir}"

    cp "${home_dir}/.git-ai/bin/git-ai" "${home_dir}/old-binary.expected"
    cp "${home_dir}/.git-ai/bin/git" "${home_dir}/old-shim.expected"
    cp "${home_dir}/.local/bin/git-ai" "${home_dir}/old-link.expected"

    if run_installer "${installer}" "${home_dir}" "${fail_at}"; then
        fail "${label}: injected ${fail_at} failure unexpectedly succeeded"
    fi

    cmp "${home_dir}/old-binary.expected" "${home_dir}/.git-ai/bin/git-ai" \
        || fail "${label}: old binary was not restored after ${fail_at}"
    cmp "${home_dir}/old-shim.expected" "${home_dir}/.git-ai/bin/git" \
        || fail "${label}: old git shim was not restored after ${fail_at}"
    cmp "${home_dir}/old-link.expected" "${home_dir}/.local/bin/git-ai" \
        || fail "${label}: old CLI link was not restored after ${fail_at}"
    assert_no_temporary_install_files "${home_dir}"
}

test_first_install_rollback_preserves_data() {
    installer=$1
    label=$2
    home_dir="${TEST_ROOT}/${label}-first-install"
    mkdir -p "${home_dir}/.git-ai/outbox"
    printf '%s\n' '{"server":"internal"}' > "${home_dir}/.git-ai/config.json"
    printf '%s\n' 'sqlite facts' > "${home_dir}/.git-ai/metrics.sqlite"
    printf '%s\n' 'pending facts' > "${home_dir}/.git-ai/outbox/pending.json"

    if run_installer "${installer}" "${home_dir}" after_cli_link_publish; then
        fail "${label}: injected first-install failure unexpectedly succeeded"
    fi

    [ ! -e "${home_dir}/.git-ai/bin/git-ai" ] \
        || fail "${label}: failed first install left git-ai binary"
    [ ! -e "${home_dir}/.git-ai/bin/git" ] \
        || fail "${label}: failed first install left git shim"
    [ ! -e "${home_dir}/.local/bin/git-ai" ] && [ ! -L "${home_dir}/.local/bin/git-ai" ] \
        || fail "${label}: failed first install left CLI link"
    [ "$(cat "${home_dir}/.git-ai/config.json")" = '{"server":"internal"}' ] \
        || fail "${label}: failed install changed config"
    [ "$(cat "${home_dir}/.git-ai/metrics.sqlite")" = 'sqlite facts' ] \
        || fail "${label}: failed install changed SQLite facts"
    [ "$(cat "${home_dir}/.git-ai/outbox/pending.json")" = 'pending facts' ] \
        || fail "${label}: failed install changed outbox facts"
    assert_no_temporary_install_files "${home_dir}"
}

test_successful_upgrade() {
    installer=$1
    label=$2
    home_dir="${TEST_ROOT}/${label}-success"
    mkdir -p "${home_dir}"
    write_old_install "${home_dir}"

    run_installer "${installer}" "${home_dir}"

    [ "$("${home_dir}/.git-ai/bin/git-ai" --version)" = 'git-ai test-new' ] \
        || fail "${label}: upgraded binary is not the new version"
    [ "$("${home_dir}/.git-ai/bin/git" --version)" = 'git-ai test-new' ] \
        || fail "${label}: existing git shim was not upgraded with the binary"
    [ -L "${home_dir}/.local/bin/git-ai" ] \
        || fail "${label}: CLI link was not published as a symlink"
    [ "$("${home_dir}/.local/bin/git-ai" --version)" = 'git-ai test-new' ] \
        || fail "${label}: CLI link does not resolve to the upgraded binary"
    assert_no_temporary_install_files "${home_dir}"
}

assert_contains() {
    file=$1
    pattern=$2
    grep -Fq "${pattern}" "${file}" \
        || fail "$(basename "${file}") is missing transactional marker: ${pattern}"
}

assert_windows_transaction_shape() {
    script=$1
    for marker in \
        'Restore-InstallTransaction' \
        'Complete-InstallTransaction' \
        '.install-backup.' \
        'GIT_AI_INSTALL_TEST_FAIL_AT' \
        'after_backups_preserved' \
        'after_binary_publish' \
        'after_shim_publish'
    do
        assert_contains "${script}" "${marker}"
    done

    backup_line=$(grep -n -F 'Move-Item -LiteralPath $finalExe -Destination $binaryBackup' "${script}" | head -n 1 | cut -d: -f1)
    publish_line=$(grep -n -F 'Move-Item -LiteralPath $tmpFile -Destination $finalExe' "${script}" | head -n 1 | cut -d: -f1)
    [ -n "${backup_line}" ] && [ -n "${publish_line}" ] && [ "${backup_line}" -lt "${publish_line}" ] \
        || fail "$(basename "${script}"): old binary is not preserved before new binary publish"

    shim_backup_line=$(grep -n -F 'Move-Item -LiteralPath $gitShim -Destination $gitShimBackup' "${script}" | head -n 1 | cut -d: -f1)
    shim_publish_line=$(grep -n -F 'Copy-Item -LiteralPath $finalExe -Destination $gitShim' "${script}" | head -n 1 | cut -d: -f1)
    [ -n "${shim_backup_line}" ] && [ -n "${shim_publish_line}" ] && [ "${shim_backup_line}" -lt "${shim_publish_line}" ] \
        || fail "$(basename "${script}"): old git shim is not preserved before new shim publish"
}

for source_installer in \
    "${REPO_ROOT}/install.sh" \
    "${REPO_ROOT}/offline-dist/git-ai-offline-v1.6.16/install.sh"
do
    label=$(basename "$(dirname "${source_installer}")")
    [ "${label}" = 'git-ai' ] && label=root
    prepared_installer="${TEST_ROOT}/${label}-install.sh"
    prepare_installer "${source_installer}" "${prepared_installer}"

    test_upgrade_rollback "${prepared_installer}" "${label}" after_binary_publish
    test_upgrade_rollback "${prepared_installer}" "${label}" after_backups_preserved
    test_upgrade_rollback "${prepared_installer}" "${label}" after_shim_publish
    test_upgrade_rollback "${prepared_installer}" "${label}" after_cli_link_publish
    test_first_install_rollback_preserves_data "${prepared_installer}" "${label}"
    test_successful_upgrade "${prepared_installer}" "${label}"
done

assert_windows_transaction_shape "${REPO_ROOT}/install.ps1"
assert_windows_transaction_shape "${REPO_ROOT}/offline-dist/git-ai-offline-v1.6.16/install.ps1"

POWERSHELL_BIN=""
if command -v pwsh >/dev/null 2>&1; then
    POWERSHELL_BIN=$(command -v pwsh)
elif command -v powershell.exe >/dev/null 2>&1; then
    POWERSHELL_BIN=$(command -v powershell.exe)
fi
if [ -n "${POWERSHELL_BIN}" ]; then
    for windows_installer in \
        "${REPO_ROOT}/install.ps1" \
        "${REPO_ROOT}/offline-dist/git-ai-offline-v1.6.16/install.ps1"
    do
        "${POWERSHELL_BIN}" -NoProfile -NonInteractive -Command \
            '& { param($path) $tokens = $null; $errors = $null; [void][System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors); if ($errors.Count -gt 0) { $errors | ForEach-Object { [Console]::Error.WriteLine($_.Message) }; exit 1 } }' \
            "${windows_installer}" \
            || fail "PowerShell parser rejected ${windows_installer}"
    done
else
    printf '%s\n' '[offline-install-test] PowerShell unavailable; Windows installers received static transaction checks only'
fi

printf '%s\n' '[offline-install-test] Transactional installer rollback tests passed'
