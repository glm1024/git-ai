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
    --version) printf '%s\n' 'git-ai 1.6.17' ;;
    exchange-nonce|install-hooks|login) exit 0 ;;
    bg) exit 0 ;;
esac
exit 0
EOF
chmod 755 "${FAKE_NEW_BINARY}"

FAKE_ARGV0_BINARY="${TEST_ROOT}/argv0-sensitive-git-ai"
cat > "${FAKE_ARGV0_BINARY}" <<'EOF'
#!/bin/sh
case "${0##*/}" in
    git-ai|git) ;;
    *)
        if [ "${1:-}" = --version ]; then
            printf '%s\n' 'git version 2.48.1'
        fi
        exit 0
        ;;
esac
case "${1:-}" in
    --version) printf '%s\n' 'git-ai 1.6.17' ;;
    exchange-nonce|install-hooks|login|bg) exit 0 ;;
esac
exit 0
EOF
chmod 755 "${FAKE_ARGV0_BINARY}"

prepare_installer() {
    source_installer=$1
    prepared_installer=$2
    awk '
        /^PINNED_VERSION=/ {
            print "PINNED_VERSION=\"__VERSION_PLACEHOLDER__\""
            next
        }
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
    leftovers=$(find "${home_dir}" \( \
        -name '*.install-backup*' -o \
        -name 'git-ai.tmp.*' -o \
        -name 'git-ai.install-staged*' -o \
        -name 'install-transaction*' -o \
        -name 'install.lock*' \
    \) 2>/dev/null || true)
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
    expected_version=${4:-}
    allow_downgrade=${5:-}
    candidate_binary=${6:-${FAKE_NEW_BINARY}}
    installer_shell=${7:-bash}
    HOME="${home_dir}" \
    SHELL=/bin/bash \
    GIT_AI_ALLOW_SUPERUSER=1 \
    GIT_AI_LOCAL_BINARY="${candidate_binary}" \
    GIT_AI_INSTALL_TEST_FAIL_AT="${fail_at}" \
    GIT_AI_INSTALL_EXPECTED_VERSION="${expected_version}" \
    GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE="${allow_downgrade}" \
        "${installer_shell}" "${installer}" >/dev/null 2>&1
}

test_argv0_sensitive_candidate() {
    installer=$1
    label=$2
    installer_shell=$3
    home_dir="${TEST_ROOT}/${label}-argv0-${installer_shell}"
    mkdir -p "${home_dir}"

    run_installer "${installer}" "${home_dir}" '' 1.6.17 '' "${FAKE_ARGV0_BINARY}" "${installer_shell}"
    [ "$("${home_dir}/.git-ai/bin/git-ai" --version)" = 'git-ai 1.6.17' ] \
        || fail "${label}: argv0-sensitive candidate failed via ${installer_shell}"
    assert_no_temporary_install_files "${home_dir}"
}

test_help_and_unknown_args_are_side_effect_free() {
    installer=$1
    label=$2
    home_dir="${TEST_ROOT}/${label}-help-guard"
    mkdir -p "${home_dir}"

    HOME="${home_dir}" sh "${installer}" --help >/dev/null 2>&1 \
        || fail "${label}: sh install.sh --help failed"
    [ ! -e "${home_dir}/.git-ai" ] \
        || fail "${label}: --help changed the install root"
    if HOME="${home_dir}" sh "${installer}" --unexpected >/dev/null 2>&1; then
        fail "${label}: unknown installer argument unexpectedly succeeded"
    fi
    [ ! -e "${home_dir}/.git-ai" ] \
        || fail "${label}: unknown installer argument changed the install root"
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

    [ "$("${home_dir}/.git-ai/bin/git-ai" --version)" = 'git-ai 1.6.17' ] \
        || fail "${label}: upgraded binary is not the new version"
    [ "$("${home_dir}/.git-ai/bin/git" --version)" = 'git-ai 1.6.17' ] \
        || fail "${label}: existing git shim was not upgraded with the binary"
    [ -L "${home_dir}/.local/bin/git-ai" ] \
        || fail "${label}: CLI link was not published as a symlink"
    [ "$("${home_dir}/.local/bin/git-ai" --version)" = 'git-ai 1.6.17' ] \
        || fail "${label}: CLI link does not resolve to the upgraded binary"
    assert_no_temporary_install_files "${home_dir}"
}

test_expected_version_gate() {
    installer=$1
    label=$2
    home_dir="${TEST_ROOT}/${label}-expected-version"
    mkdir -p "${home_dir}"
    write_old_install "${home_dir}"

    if run_installer "${installer}" "${home_dir}" '' 1.6.18; then
        fail "${label}: mismatched expected version unexpectedly succeeded"
    fi
    [ "$(cat "${home_dir}/.git-ai/bin/git-ai")" = 'old binary bytes' ] \
        || fail "${label}: expected-version rejection did not preserve the old binary"
    assert_no_temporary_install_files "${home_dir}"
}

write_versioned_old_install() {
    home_dir=$1
    mkdir -p "${home_dir}/.git-ai/bin" "${home_dir}/.local/bin"
    for target in \
        "${home_dir}/.git-ai/bin/git-ai" \
        "${home_dir}/.git-ai/bin/git" \
        "${home_dir}/.local/bin/git-ai"
    do
        printf '%s\n' '#!/bin/sh' > "${target}"
        printf '%s\n' "printf '%s\\n' 'git-ai 2.0.0'" >> "${target}"
        chmod 755 "${target}"
    done
}

test_downgrade_gate() {
    installer=$1
    label=$2
    home_dir="${TEST_ROOT}/${label}-downgrade-gate"
    mkdir -p "${home_dir}"
    write_versioned_old_install "${home_dir}"

    if run_installer "${installer}" "${home_dir}"; then
        fail "${label}: schema-unsafe downgrade unexpectedly succeeded"
    fi
    [ "$("${home_dir}/.git-ai/bin/git-ai" --version)" = 'git-ai 2.0.0' ] \
        || fail "${label}: downgrade rejection did not preserve the old binary"

    run_installer "${installer}" "${home_dir}" '' '' 1
    [ "$("${home_dir}/.git-ai/bin/git-ai" --version)" = 'git-ai 1.6.17' ] \
        || fail "${label}: explicit schema-unsafe downgrade override did not install the candidate"
    assert_no_temporary_install_files "${home_dir}"
}

test_crash_journal_recovery() {
    installer=$1
    label=$2
    home_dir="${TEST_ROOT}/${label}-crash-recovery"
    mkdir -p "${home_dir}"
    write_old_install "${home_dir}"

    mv "${home_dir}/.git-ai/bin/git-ai" "${home_dir}/.git-ai/bin/git-ai.install-backup"
    mv "${home_dir}/.git-ai/bin/git" "${home_dir}/.git-ai/bin/git.install-backup"
    mv "${home_dir}/.local/bin/git-ai" "${home_dir}/.local/bin/git-ai.install-backup"
    cp "${FAKE_NEW_BINARY}" "${home_dir}/.git-ai/bin/git-ai"
    cp "${FAKE_NEW_BINARY}" "${home_dir}/.git-ai/bin/git"
    ln -s "${home_dir}/.git-ai/bin/git-ai" "${home_dir}/.local/bin/git-ai"
    {
        printf '%s\n' 'format=1'
        printf '%s\n' 'phase=prepared'
        printf '%s\n' 'binary_was_present=true'
        printf '%s\n' 'git_shim_was_present=true'
        printf '%s\n' 'cli_link_was_present=true'
    } > "${home_dir}/.git-ai/install-transaction"
    mkdir "${home_dir}/.git-ai/install.lock.d"
    printf '%s\n' 999999 > "${home_dir}/.git-ai/install.lock.d/pid"

    if run_installer "${installer}" "${home_dir}" after_backups_preserved; then
        fail "${label}: injected failure after crash recovery unexpectedly succeeded"
    fi
    [ "$(cat "${home_dir}/.git-ai/bin/git-ai")" = 'old binary bytes' ] \
        || fail "${label}: crash journal did not restore the old binary before retry"
    [ "$(cat "${home_dir}/.git-ai/bin/git")" = 'old git shim bytes' ] \
        || fail "${label}: crash journal did not restore the old shim before retry"
    [ "$(cat "${home_dir}/.local/bin/git-ai")" = 'old cli link bytes' ] \
        || fail "${label}: crash journal did not restore the old CLI link before retry"
    assert_no_temporary_install_files "${home_dir}"
}

test_ambiguous_crash_journal_fails_closed() {
    installer=$1
    label=$2
    home_dir="${TEST_ROOT}/${label}-ambiguous-crash-recovery"
    mkdir -p "${home_dir}"
    write_old_install "${home_dir}"
    {
        printf '%s\n' 'format=1'
        printf '%s\n' 'phase=prepared'
        printf '%s\n' 'binary_was_present=true'
        printf '%s\n' 'git_shim_was_present=true'
        printf '%s\n' 'cli_link_was_present=true'
    } > "${home_dir}/.git-ai/install-transaction"

    if run_installer "${installer}" "${home_dir}"; then
        fail "${label}: ambiguous prepared journal unexpectedly continued installation"
    fi
    [ "$(cat "${home_dir}/.git-ai/bin/git-ai")" = 'old binary bytes' ] \
        || fail "${label}: ambiguous recovery changed the current binary"
    [ "$(cat "${home_dir}/.git-ai/bin/git")" = 'old git shim bytes' ] \
        || fail "${label}: ambiguous recovery changed the current shim"
    [ "$(cat "${home_dir}/.local/bin/git-ai")" = 'old cli link bytes' ] \
        || fail "${label}: ambiguous recovery changed the current CLI path"
    [ -f "${home_dir}/.git-ai/install-transaction" ] \
        || fail "${label}: ambiguous recovery did not retain its journal for inspection"
    [ ! -e "${home_dir}/.git-ai/install.lock.d" ] \
        || fail "${label}: ambiguous recovery left the installer lock held"
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
        '.install-backup' \
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

run_unix_installer_suite() {
    source_installer=$1
    label=$2
    prepared_installer="${TEST_ROOT}/${label}-install.sh"
    prepare_installer "${source_installer}" "${prepared_installer}"

    test_upgrade_rollback "${prepared_installer}" "${label}" after_binary_publish
    test_upgrade_rollback "${prepared_installer}" "${label}" after_backups_preserved
    test_upgrade_rollback "${prepared_installer}" "${label}" after_shim_publish
    test_upgrade_rollback "${prepared_installer}" "${label}" after_cli_link_publish
    test_first_install_rollback_preserves_data "${prepared_installer}" "${label}"
    test_successful_upgrade "${prepared_installer}" "${label}"
    test_help_and_unknown_args_are_side_effect_free "${prepared_installer}" "${label}"
    test_argv0_sensitive_candidate "${prepared_installer}" "${label}" bash
    test_argv0_sensitive_candidate "${prepared_installer}" "${label}" sh
    test_expected_version_gate "${prepared_installer}" "${label}"
    test_downgrade_gate "${prepared_installer}" "${label}"
    test_crash_journal_recovery "${prepared_installer}" "${label}"
    test_ambiguous_crash_journal_fails_closed "${prepared_installer}" "${label}"
}

assert_windows_release_shape() {
    windows_installer=$1
    assert_windows_transaction_shape "${windows_installer}"
    for marker in \
        'install-transaction.json' \
        'install.lock' \
        'GIT_AI_INSTALL_EXPECTED_VERSION' \
        'GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE' \
        'GIT_AI_UPDATE_RECEIPT_PATH' \
        'Write-UpgradeReceiptIfRequested' \
        'MoveFileExW' \
        'MOVEFILE_WRITE_THROUGH' \
        "Join-Path \$stagingDir 'git-ai.exe'" \
        'Initialize-StagingDirectory' \
        'after_committed_journal_before_receipt' \
        'Complete-RecoveredUpgradeReceipt' \
        "@('-h', '--help')"
    do
        assert_contains "${windows_installer}" "${marker}"
    done
    assert_contains "${windows_installer}" 'Interrupted install is ambiguous'
    complete_line=$(grep -n -F 'function Complete-InstallTransaction {' "${windows_installer}" | cut -d: -f1)
    receipt_line=$(grep -n -F 'Write-UpgradeReceiptIfRequested -InstalledVersion $InstalledVersion' "${windows_installer}" | cut -d: -f1)
    unlock_line=$(awk -v start="${complete_line}" 'NR > start && /Exit-InstallLock/ { print NR; exit }' "${windows_installer}")
    [ -n "${receipt_line}" ] && [ -n "${unlock_line}" ] && [ "${receipt_line}" -lt "${unlock_line}" ] \
        || fail 'Windows upgrade receipt must be durably published before releasing the installer lock'
}

run_unix_installer_suite "${REPO_ROOT}/install.sh" source
assert_windows_release_shape "${REPO_ROOT}/install.ps1"

PACKAGED_DIST_UNDER_TEST=${GIT_AI_PACKAGED_DIST_UNDER_TEST:-}
if [ -n "${PACKAGED_DIST_UNDER_TEST}" ]; then
    [ -f "${PACKAGED_DIST_UNDER_TEST}/install.sh" ] \
        || fail "Packaged installer is missing: ${PACKAGED_DIST_UNDER_TEST}/install.sh"
    [ -f "${PACKAGED_DIST_UNDER_TEST}/install.ps1" ] \
        || fail "Packaged installer is missing: ${PACKAGED_DIST_UNDER_TEST}/install.ps1"
    run_unix_installer_suite "${PACKAGED_DIST_UNDER_TEST}/install.sh" packaged
    assert_windows_release_shape "${PACKAGED_DIST_UNDER_TEST}/install.ps1"
fi

POWERSHELL_BIN=""
if command -v pwsh >/dev/null 2>&1; then
    POWERSHELL_BIN=$(command -v pwsh)
elif command -v powershell.exe >/dev/null 2>&1; then
    POWERSHELL_BIN=$(command -v powershell.exe)
fi
if [ -n "${POWERSHELL_BIN}" ]; then
    parse_windows_installer() {
        windows_installer=$1
        "${POWERSHELL_BIN}" -NoProfile -NonInteractive -Command \
            '& { param($path) $tokens = $null; $errors = $null; [void][System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors); if ($errors.Count -gt 0) { $errors | ForEach-Object { [Console]::Error.WriteLine($_.Message) }; exit 1 } }' \
            "${windows_installer}" \
            || fail "PowerShell parser rejected ${windows_installer}"
    }
    parse_windows_installer "${REPO_ROOT}/install.ps1"
    if [ -n "${PACKAGED_DIST_UNDER_TEST}" ]; then
        parse_windows_installer "${PACKAGED_DIST_UNDER_TEST}/install.ps1"
    fi
else
    printf '%s\n' '[offline-install-test] PowerShell unavailable; Windows installer received static transaction checks only'
fi

printf '%s\n' '[offline-install-test] Transactional installer rollback tests passed'
