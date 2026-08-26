if (@($args).Count -gt 0) {
    if (@($args).Count -eq 1 -and $args[0] -in @('-h', '--help')) {
        $usage = @'
Install git-ai for the current user.

Usage: .\install.ps1

Options:
  -h, --help  Show this help without downloading or changing local files.
'@
        Write-Output $usage
        exit 0
    }
    [Console]::Error.WriteLine("Error: unknown installer argument(s): $($args -join ' ')")
    exit 2
}

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$script:InstallTransactionActive = $false

if (-not ([System.Management.Automation.PSTypeName]'GitAiInstaller.NativeFile').Type) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

namespace GitAiInstaller
{
    public static class NativeFile
    {
        public const uint MOVEFILE_REPLACE_EXISTING = 0x1;
        public const uint MOVEFILE_WRITE_THROUGH = 0x8;

        [DllImport("kernel32.dll", EntryPoint = "MoveFileExW", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool MoveFileEx(string existingFileName, string newFileName, uint flags);
    }
}
'@
}

function Start-DaemonIfRequested {
    if ($env:GIT_AI_RESTART_DAEMON_AFTER_INSTALL -ne '1') {
        return
    }

    $daemonExe = Join-Path $HOME '.git-ai\bin\git-ai.exe'
    if (-not (Test-Path $daemonExe)) {
        Write-Warning 'Warning: Failed to locate git-ai.exe for daemon restart after install.'
        return
    }

    try {
        & $daemonExe bg start *> $null
    } catch {
        Write-Warning 'Warning: Failed to restart git-ai background service automatically.'
    }
}

function Write-ErrorAndExit {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    if ($script:InstallTransactionActive) {
        throw [System.InvalidOperationException]::new($Message)
    }
    Write-Host "Error: $Message" -ForegroundColor Red
    Start-DaemonIfRequested
    exit 1
}

function Write-Success {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    Write-Host $Message -ForegroundColor Green
}

function Write-Warning {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    Write-Host $Message -ForegroundColor Yellow
}

function Get-NormalizedVersion {
    param(
        [Parameter(Mandatory = $false)][AllowEmptyString()][string]$Text
    )

    if ([string]::IsNullOrWhiteSpace($Text)) {
        return $null
    }
    $match = [regex]::Match($Text, '(?<![0-9])([0-9]+\.[0-9]+\.[0-9]+(?:\.[0-9]+)*)(?![0-9])')
    if (-not $match.Success) {
        return $null
    }
    return $match.Groups[1].Value
}

function Compare-NumericVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )

    $leftParts = @($Left.Split('.') | ForEach-Object { [UInt64]$_ })
    $rightParts = @($Right.Split('.') | ForEach-Object { [UInt64]$_ })
    $count = [Math]::Max($leftParts.Count, $rightParts.Count)
    for ($index = 0; $index -lt $count; $index++) {
        $leftValue = if ($index -lt $leftParts.Count) { $leftParts[$index] } else { 0 }
        $rightValue = if ($index -lt $rightParts.Count) { $rightParts[$index] } else { 0 }
        if ($leftValue -gt $rightValue) { return 1 }
        if ($leftValue -lt $rightValue) { return -1 }
    }
    return 0
}

function Write-DurableUtf8File {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Content
    )

    $tempPath = "$Path.tmp.$PID"
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    $bytes = $utf8NoBom.GetBytes($Content)
    $stream = $null
    try {
        $stream = [System.IO.File]::Open(
            $tempPath,
            [System.IO.FileMode]::Create,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
        $stream.Dispose()
        $stream = $null

        $moveFlags = [uint32](
            [GitAiInstaller.NativeFile]::MOVEFILE_REPLACE_EXISTING -bor
            [GitAiInstaller.NativeFile]::MOVEFILE_WRITE_THROUGH
        )
        if (-not [GitAiInstaller.NativeFile]::MoveFileEx($tempPath, $Path, $moveFlags)) {
            $nativeError = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            throw [System.ComponentModel.Win32Exception]::new(
                $nativeError,
                "Could not durably publish file at $Path"
            )
        }
    } finally {
        if ($null -ne $stream) {
            $stream.Dispose()
        }
        Remove-Item -LiteralPath $tempPath -Force -ErrorAction SilentlyContinue
    }
}

function Normalize-PathString {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    try {
        return ([IO.Path]::GetFullPath($Path.Trim())).TrimEnd('\').ToLowerInvariant()
    } catch {
        return ($Path.Trim()).TrimEnd('\').ToLowerInvariant()
    }
}

function Test-FileAvailable {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    try {
        $stream = [System.IO.File]::Open($Path, 'Open', 'Write', 'None')
        $stream.Close()
        return $true
    } catch {
        return $false
    }
}

function Stop-GitAiBackgroundService {
    param(
        [Parameter(Mandatory = $true)][string]$GitAiExe,
        [Parameter(Mandatory = $false)][switch]$Hard
    )

    if (-not (Test-Path -LiteralPath $GitAiExe)) {
        return $false
    }

    $args = @('bg', 'shutdown')
    if ($Hard) {
        $args += '--hard'
    }

    try {
        & $GitAiExe @args *> $null
        return $LASTEXITCODE -eq 0
    } catch {
        return $false
    }
}

function Get-GitAiManagedProcesses {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir
    )

    $targetPaths = @(
        (Normalize-PathString (Join-Path $InstallDir 'git-ai.exe')),
        (Normalize-PathString (Join-Path $InstallDir 'git.exe'))
    )

    $processes = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
            $_.ProcessId -ne $PID -and
            $_.ExecutablePath -and
            ($targetPaths -contains (Normalize-PathString $_.ExecutablePath))
        })

    return $processes
}

function Stop-GitAiManagedProcesses {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir
    )

    $processes = @(Get-GitAiManagedProcesses -InstallDir $InstallDir)
    if ($processes.Count -eq 0) {
        return $false
    }

    $pids = @($processes | Sort-Object ProcessId -Unique | Select-Object -ExpandProperty ProcessId)
    Write-Warning ("Stopping lingering git-ai processes: {0}" -f ($pids -join ', '))

    foreach ($managedPid in $pids) {
        try {
            Stop-Process -Id $managedPid -Force -ErrorAction Stop
        } catch { }
    }

    return $true
}

function Wait-ForFileAvailable {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [Parameter(Mandatory = $false)][int]$MaxWaitSeconds = 300,
        [Parameter(Mandatory = $false)][int]$RetryIntervalSeconds = 5,
        [Parameter(Mandatory = $false)][int]$ForceKillAfterSeconds = 20
    )

    $elapsed = 0
    $gitAiExe = Join-Path $InstallDir 'git-ai.exe'

    [void](Stop-GitAiBackgroundService -GitAiExe $gitAiExe)

    while ($elapsed -lt $MaxWaitSeconds) {
        if (Test-FileAvailable -Path $Path) {
            return $true
        }

        if ($elapsed -ge $ForceKillAfterSeconds) {
            [void](Stop-GitAiBackgroundService -GitAiExe $gitAiExe -Hard)
            [void](Stop-GitAiManagedProcesses -InstallDir $InstallDir)
        }

        if (-not (Test-FileAvailable -Path $Path)) {
            if ($elapsed -eq 0) {
                Write-Host "Waiting for file to be available: $Path" -ForegroundColor Yellow
            }
            Start-Sleep -Seconds $RetryIntervalSeconds
            $elapsed += $RetryIntervalSeconds
        }
    }
    return $false
}

function Verify-Checksum {
    param(
        [Parameter(Mandatory = $true)][string]$File,
        [Parameter(Mandatory = $true)][string]$BinaryName
    )

    # Skip verification if no checksums are embedded
    if ($EmbeddedChecksums -eq '__CHECKSUMS_PLACEHOLDER__') {
        return
    }

    # Extract expected checksum for this binary
    $expected = $null
    $entries = $EmbeddedChecksums -split '\|'
    foreach ($entry in $entries) {
        if ($entry -match "^([0-9a-fA-F]+)\s+$([regex]::Escape($BinaryName))$") {
            $expected = $Matches[1]
            break
        }
    }

    if (-not $expected) {
        Write-ErrorAndExit "No checksum found for $BinaryName"
    }

    # Calculate actual checksum
    $hashCommand = Get-Command Get-FileHash -ErrorAction SilentlyContinue
    if ($null -ne $hashCommand) {
        $actual = (Get-FileHash -Path $File -Algorithm SHA256).Hash.ToLower()
    } else {
        $stream = [System.IO.File]::OpenRead($File)
        try {
            $sha256 = [System.Security.Cryptography.SHA256]::Create()
            $hashBytes = $sha256.ComputeHash($stream)
            $actual = ([System.BitConverter]::ToString($hashBytes)).Replace('-', '').ToLower()
        } finally {
            $stream.Dispose()
            if ($sha256) {
                $sha256.Dispose()
            }
        }
    }

    if ($expected -ne $actual) {
        Remove-Item -Force -ErrorAction SilentlyContinue $File
        Write-ErrorAndExit "Checksum verification failed for $BinaryName`nExpected: $expected`nActual:   $actual"
    }

    Write-Success "Checksum verified for $BinaryName"
}

# GitHub repository details
# Replaced during release builds with the actual repository (e.g., "git-ai-project/git-ai")
# When set to __REPO_PLACEHOLDER__, defaults to "git-ai-project/git-ai"
$Repo = '__REPO_PLACEHOLDER__'
if ($Repo -eq '__REPO_PLACEHOLDER__') {
    $Repo = 'git-ai-project/git-ai'
}

# Version placeholder - replaced during release builds with actual version (e.g., "v1.0.24")
# When set to __VERSION_PLACEHOLDER__, defaults to "latest"
$PinnedVersion = '__VERSION_PLACEHOLDER__'

# Embedded checksums - replaced during release builds with actual SHA256 checksums
# Format: "hash  filename|hash  filename|..." (pipe-separated)
# When set to __CHECKSUMS_PLACEHOLDER__, checksum verification is skipped
$EmbeddedChecksums = '__CHECKSUMS_PLACEHOLDER__'

# Ensure TLS 1.2 for GitHub downloads on older PowerShell versions
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch { }

function Get-Architecture {
    try {
        $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
        switch ($arch) {
            'X64' { return 'x64' }
            'Arm64' { return 'arm64' }
            default { return $null }
        }
    } catch {
        $pa = $env:PROCESSOR_ARCHITECTURE
        if ($pa -match 'ARM64') { return 'arm64' }
        elseif ($pa -match '64') { return 'x64' }
        else { return $null }
    }
}

# Detect architecture and OS
$arch = Get-Architecture
if (-not $arch) { Write-ErrorAndExit "Unsupported architecture: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)" }
$os = 'windows'

# Determine binary name and download URLs
$binaryName = "git-ai-$os-$arch"

# Determine release tag
# Priority: 1. Local binary override, 2. Pinned version (for release builds), 3. Environment variable, 4. "latest"
if (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_LOCAL_BINARY)) {
    $releaseTag = 'local'
} elseif ($PinnedVersion -ne '__VERSION_PLACEHOLDER__') {
    # Version-pinned install script from a release
    $releaseTag = $PinnedVersion
    $downloadUrlExe = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName"
} elseif (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_RELEASE_TAG) -and $env:GIT_AI_RELEASE_TAG -ne 'latest') {
    # Environment variable override
    $releaseTag = $env:GIT_AI_RELEASE_TAG
    $downloadUrlExe = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName"
} else {
    # Default to latest
    $releaseTag = 'latest'
    $downloadUrlExe = "https://github.com/$Repo/releases/latest/download/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/latest/download/$binaryName"
}

# ============================================================
# Warn when installing as Administrator (not recommended).
# Running elevated creates files that normal-user processes
# cannot access, causing persistent daemon lock failures.
# ============================================================
$isElevated = $false
try {
    # Detect explicit UAC elevation ("Run as Administrator") via TokenElevationType.
    # Type 1 (Default) = no split token (UAC disabled or built-in Admin) -> no warn
    # Type 2 (Full)    = elevated half of a split token -> WARN (this is the danger case)
    # Type 3 (Limited) = non-elevated half of a split token -> no warn
    # We only warn on type 2: user explicitly elevated, so files will be admin-owned
    # but normal processes won't be, causing the daemon.lock mismatch from issue #1287.
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class GitAiElevation {
    [DllImport("advapi32.dll", SetLastError=true)]
    static extern bool OpenProcessToken(IntPtr h, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError=true)]
    static extern bool GetTokenInformation(IntPtr token, int cls, ref int info, int len, out int ret);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr h);
    public static bool IsElevated() {
        IntPtr tok;
        if (!OpenProcessToken(GetCurrentProcess(), 0x0008, out tok)) return false;
        try {
            int elevType = 0; int sz;
            // TokenElevationType = class 18; returns 1/2/3
            if (!GetTokenInformation(tok, 18, ref elevType, 4, out sz)) return false;
            return elevType == 2; // TokenElevationTypeFull
        } finally { CloseHandle(tok); }
    }
}
"@ -ErrorAction SilentlyContinue
    $isElevated = [GitAiElevation]::IsElevated()
} catch { }

if ($isElevated -and $env:GIT_AI_ALLOW_SUPERUSER -ne '1') {
    # Auto-allow in CI environments and daemon-triggered self-updates
    $isCi = $env:CI -or $env:GITHUB_ACTIONS -or $env:GITLAB_CI -or $env:JENKINS_URL `
        -or $env:BUILDKITE -or $env:CIRCLECI -or $env:CODEBUILD_BUILD_ID `
        -or $env:AGENT_OS -or $env:KUBERNETES_SERVICE_HOST `
        -or $env:GIT_AI_DAEMON_UPGRADE -or $env:container

    if (-not $isCi) {
        Write-Host ''
        Write-Host 'Warning: installing git-ai as Administrator is not recommended.' -ForegroundColor Yellow
        Write-Host ''
        Write-Host 'Running with elevated privileges creates files owned by Administrator that'
        Write-Host 'become inaccessible to your normal user account, causing persistent daemon'
        Write-Host 'lock failures. A future version may refuse to install in this configuration.'
        Write-Host ''
        Write-Host 'To suppress this warning, either:'
        Write-Host '  - Run this installer from a normal (non-elevated) PowerShell window (recommended), or'
        Write-Host '  - Set $env:GIT_AI_ALLOW_SUPERUSER = "1"' -ForegroundColor Yellow
        Write-Host ''
    }
    # Propagate to child git-ai invocations (install-hooks, exchange-nonce, login)
    $env:GIT_AI_ALLOW_SUPERUSER = '1'
}

# Install directory: %USERPROFILE%\.git-ai\bin. Stable backups and a durable
# journal make executable publication recoverable after process termination or
# host restart. Configuration, SQLite files, and outbox data are never rollback
# targets.
$installRoot = Join-Path $HOME '.git-ai'
$installDir = Join-Path $installRoot 'bin'
$finalExe = Join-Path $installDir 'git-ai.exe'
$gitShim = Join-Path $installDir 'git.exe'
$stagingDir = Join-Path $installDir '.git-ai.install-staged'
$tmpFile = Join-Path $stagingDir 'git-ai.exe'
$binaryBackup = "$finalExe.install-backup"
$gitShimBackup = "$gitShim.install-backup"
$installJournal = Join-Path $installRoot 'install-transaction.json'
$installLockPath = Join-Path $installRoot 'install.lock'

$script:InstallRootCreated = -not (Test-Path -LiteralPath $installRoot)
$script:InstallDirCreated = -not (Test-Path -LiteralPath $installDir)
$script:InstallLockStream = $null
$script:BinaryPreserved = $false
$script:GitShimPreserved = $false
$script:BinaryPublishAttempted = $false
$script:GitShimPublishAttempted = $false
$script:GitShimWasPresent = Test-Path -LiteralPath $gitShim
$script:BinaryWasPresent = Test-Path -LiteralPath $finalExe
$script:PreviousVersion = $null
$script:ExpectedInstallVersion = $null
$script:UpgradeReceiptRequested = -not [string]::IsNullOrWhiteSpace($env:GIT_AI_UPDATE_RECEIPT_PATH)
$script:UpgradeReceiptPath = if ($script:UpgradeReceiptRequested) {
    [System.IO.Path]::GetFullPath($env:GIT_AI_UPDATE_RECEIPT_PATH)
} else {
    ''
}

function Remove-StagedCandidate {
    Remove-Item -LiteralPath $tmpFile -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $stagingDir -Force -ErrorAction SilentlyContinue
}

function Initialize-StagingDirectory {
    if (Test-Path -LiteralPath $stagingDir) {
        $stagingItem = Get-Item -LiteralPath $stagingDir -Force -ErrorAction Stop
        if (-not $stagingItem.PSIsContainer -or
            (($stagingItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)) {
            throw [System.InvalidOperationException]::new(
                "Installer staging path requires manual inspection: $stagingDir"
            )
        }
        Remove-StagedCandidate
        if (Test-Path -LiteralPath $stagingDir) {
            throw [System.InvalidOperationException]::new(
                "Installer staging directory is not empty: $stagingDir"
            )
        }
    }
    New-Item -ItemType Directory -Path $stagingDir -ErrorAction Stop | Out-Null
}

function Exit-InstallLock {
    if ($null -eq $script:InstallLockStream) {
        return
    }
    try {
        $script:InstallLockStream.Dispose()
    } finally {
        $script:InstallLockStream = $null
        Remove-Item -LiteralPath $installLockPath -Force -ErrorAction SilentlyContinue
    }
}

function Enter-InstallLock {
    New-Item -ItemType Directory -Force -Path $installRoot | Out-Null
    try {
        $script:InstallLockStream = [System.IO.File]::Open(
            $installLockPath,
            [System.IO.FileMode]::OpenOrCreate,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
    } catch {
        throw [System.InvalidOperationException]::new(
            "Another git-ai installer is running, or the installer lock cannot be opened: $installLockPath"
        )
    }

    $lockBytes = (New-Object System.Text.UTF8Encoding($false)).GetBytes("$PID`n")
    $script:InstallLockStream.SetLength(0)
    $script:InstallLockStream.Write($lockBytes, 0, $lockBytes.Length)
    $script:InstallLockStream.Flush($true)
}

function Write-InstallJournal {
    param(
        [Parameter(Mandatory = $true)][ValidateSet('prepared', 'committed')][string]$Phase
    )
    $journal = [ordered]@{
        format = 2
        phase = $Phase
        binary_was_present = [bool]$script:BinaryWasPresent
        git_shim_was_present = [bool]$script:GitShimWasPresent
        upgrade_receipt_requested = [bool]$script:UpgradeReceiptRequested
        upgrade_receipt_path = [string]$script:UpgradeReceiptPath
        expected_version = [string]$script:ExpectedInstallVersion
        release_tag = if ($script:UpgradeReceiptRequested) { [string]$releaseTag } else { '' }
    }
    Write-DurableUtf8File -Path $installJournal -Content ($journal | ConvertTo-Json -Compress)
}

function Restore-RecoveredPath {
    param(
        [Parameter(Mandatory = $true)][string]$FinalPath,
        [Parameter(Mandatory = $true)][string]$BackupPath,
        [Parameter(Mandatory = $true)][bool]$WasPresent,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ($WasPresent) {
        if (Test-Path -LiteralPath $BackupPath) {
            if (Test-Path -LiteralPath $FinalPath) {
                Remove-Item -LiteralPath $FinalPath -Force -ErrorAction Stop
            }
            Move-Item -LiteralPath $BackupPath -Destination $FinalPath -ErrorAction Stop
        } else {
            throw [System.InvalidOperationException]::new(
                "Interrupted install is ambiguous for $Label: the recovery journal says an old path existed, but no backup is present. The current path may be either old or newly published."
            )
        }
    } else {
        if (Test-Path -LiteralPath $BackupPath) {
            throw [System.InvalidOperationException]::new(
                "Unexpected $Label backup requires manual inspection: $BackupPath"
            )
        }
        if (Test-Path -LiteralPath $FinalPath) {
            Remove-Item -LiteralPath $FinalPath -Force -ErrorAction Stop
        }
    }
}

function Recover-InterruptedInstall {
    foreach ($backup in @($binaryBackup, $gitShimBackup)) {
        if (Test-Path -LiteralPath $backup) {
            $item = Get-Item -LiteralPath $backup -Force
            if ($item.PSIsContainer) {
                throw [System.InvalidOperationException]::new(
                    "Installer backup path is a directory: $backup"
                )
            }
        }
    }

    if (-not (Test-Path -LiteralPath $installJournal)) {
        foreach ($backup in @($binaryBackup, $gitShimBackup)) {
            if (Test-Path -LiteralPath $backup) {
                throw [System.InvalidOperationException]::new(
                    "Installer backup exists without a recovery journal: $backup"
                )
            }
        }
        Remove-StagedCandidate
        return
    }

    try {
        $journal = Get-Content -LiteralPath $installJournal -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw [System.InvalidOperationException]::new(
            "Unsupported or corrupt installer recovery journal: $installJournal"
        )
    }
    if ($journal.format -ne 2 -or $journal.phase -notin @('prepared', 'committed')) {
        throw [System.InvalidOperationException]::new(
            "Unsupported or corrupt installer recovery journal: $installJournal"
        )
    }
    if ($journal.binary_was_present -isnot [bool] -or
        $journal.git_shim_was_present -isnot [bool] -or
        $journal.upgrade_receipt_requested -isnot [bool] -or
        $journal.upgrade_receipt_path -isnot [string] -or
        $journal.expected_version -isnot [string] -or
        $journal.release_tag -isnot [string]) {
        throw [System.InvalidOperationException]::new(
            "Unsupported or corrupt installer recovery journal: $installJournal"
        )
    }
    if ([bool]$journal.upgrade_receipt_requested -and
        ([string]::IsNullOrWhiteSpace([string]$journal.upgrade_receipt_path) -or
         [string]::IsNullOrWhiteSpace([string]$journal.expected_version) -or
         [string]::IsNullOrWhiteSpace([string]$journal.release_tag))) {
        throw [System.InvalidOperationException]::new(
            "Committed upgrade receipt metadata is incomplete in $installJournal"
        )
    }

    if ($journal.phase -eq 'committed') {
        Complete-RecoveredUpgradeReceipt -Journal $journal
        foreach ($backup in @($binaryBackup, $gitShimBackup)) {
            Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
            if (Test-Path -LiteralPath $backup) {
                throw [System.InvalidOperationException]::new(
                    "Could not finish cleanup from the previous install: $backup"
                )
            }
        }
    } else {
        Restore-RecoveredPath -FinalPath $gitShim -BackupPath $gitShimBackup `
            -WasPresent ([bool]$journal.git_shim_was_present) -Label 'git shim'
        Restore-RecoveredPath -FinalPath $finalExe -BackupPath $binaryBackup `
            -WasPresent ([bool]$journal.binary_was_present) -Label 'git-ai binary'
        Write-Success 'Recovered an interrupted git-ai install before continuing'
    }

    Remove-StagedCandidate
    Remove-Item -LiteralPath $installJournal -Force -ErrorAction Stop
}

function Write-UpgradeReceipt {
    param(
        [Parameter(Mandatory = $true)][string]$ReceiptPath,
        [Parameter(Mandatory = $true)][string]$InstalledVersion,
        [Parameter(Mandatory = $true)][string]$ExpectedVersion,
        [Parameter(Mandatory = $true)][string]$ReleaseTag
    )

    if ([string]::IsNullOrWhiteSpace($ReceiptPath) -or
        [string]::IsNullOrWhiteSpace($ExpectedVersion) -or
        [string]::IsNullOrWhiteSpace($ReleaseTag)) {
        throw [System.InvalidOperationException]::new(
            'Cannot write an upgrade receipt without its path, expected version, and release tag'
        )
    }
    if ($InstalledVersion -ne $ExpectedVersion) {
        throw [System.InvalidOperationException]::new(
            "Cannot write upgrade receipt: expected $ExpectedVersion, installed $InstalledVersion"
        )
    }
    $releaseVersion = Get-NormalizedVersion -Text $ReleaseTag
    if ($releaseVersion -ne $ExpectedVersion) {
        throw [System.InvalidOperationException]::new(
            "Cannot write upgrade receipt: release tag $ReleaseTag does not match expected version $ExpectedVersion"
        )
    }

    $receipt = [ordered]@{
        format = 1
        expected_version = $ExpectedVersion
        installed_version = $InstalledVersion
        release_tag = $ReleaseTag
        completed_at_utc = [DateTime]::UtcNow.ToString('o')
    }
    $receiptParent = Split-Path -Parent $ReceiptPath
    if (-not [string]::IsNullOrWhiteSpace($receiptParent)) {
        New-Item -ItemType Directory -Force -Path $receiptParent | Out-Null
    }
    Write-DurableUtf8File -Path $ReceiptPath -Content ($receipt | ConvertTo-Json -Compress)

    try {
        $written = Get-Content -LiteralPath $ReceiptPath -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw [System.InvalidOperationException]::new(
            "Could not read back upgrade receipt at $ReceiptPath"
        )
    }
    if ($written.format -ne 1 -or
        $written.expected_version -ne $ExpectedVersion -or
        $written.installed_version -ne $InstalledVersion -or
        $written.release_tag -ne $ReleaseTag -or
        [string]::IsNullOrWhiteSpace([string]$written.completed_at_utc)) {
        throw [System.InvalidOperationException]::new(
            "Upgrade receipt failed exact read-back verification at $ReceiptPath"
        )
    }
}

function Write-UpgradeReceiptIfRequested {
    param(
        [Parameter(Mandatory = $true)][string]$InstalledVersion,
        [Parameter(Mandatory = $false)][AllowEmptyString()][string]$ExpectedVersion
    )

    if (-not $script:UpgradeReceiptRequested) {
        return
    }
    Write-UpgradeReceipt -ReceiptPath $script:UpgradeReceiptPath `
        -InstalledVersion $InstalledVersion -ExpectedVersion $ExpectedVersion -ReleaseTag $releaseTag
}

function Complete-RecoveredUpgradeReceipt {
    param(
        [Parameter(Mandatory = $true)]$Journal
    )

    if (-not [bool]$Journal.upgrade_receipt_requested) {
        return
    }
    if (-not (Test-Path -LiteralPath $finalExe)) {
        throw [System.InvalidOperationException]::new(
            "Committed install is missing git-ai.exe; recovery journal retained at $installJournal"
        )
    }
    try {
        $versionOutput = ((& $finalExe --version 2>&1) | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) {
            throw [System.InvalidOperationException]::new('git-ai.exe returned a non-zero version status')
        }
    } catch {
        throw [System.InvalidOperationException]::new(
            "Could not validate the committed binary before recovering its upgrade receipt: $($_.Exception.Message)"
        )
    }
    $installedVersion = Get-NormalizedVersion -Text $versionOutput
    if ($installedVersion -ne [string]$Journal.expected_version) {
        throw [System.InvalidOperationException]::new(
            "Committed binary version $installedVersion does not match journal version $($Journal.expected_version)"
        )
    }

    if ([bool]$Journal.git_shim_was_present) {
        if (-not (Test-Path -LiteralPath $gitShim)) {
            throw [System.InvalidOperationException]::new(
                "Committed install is missing git.exe; recovery journal retained at $installJournal"
            )
        }
        $binaryHash = (Get-FileHash -LiteralPath $finalExe -Algorithm SHA256 -ErrorAction Stop).Hash
        $shimHash = (Get-FileHash -LiteralPath $gitShim -Algorithm SHA256 -ErrorAction Stop).Hash
        if ($binaryHash -ne $shimHash) {
            throw [System.InvalidOperationException]::new(
                "Committed git.exe does not match git-ai.exe; recovery journal retained at $installJournal"
            )
        }
    } elseif (Test-Path -LiteralPath $gitShim) {
        throw [System.InvalidOperationException]::new(
            "Committed install unexpectedly contains git.exe; recovery journal retained at $installJournal"
        )
    }

    Write-UpgradeReceipt -ReceiptPath ([string]$Journal.upgrade_receipt_path) `
        -InstalledVersion $installedVersion `
        -ExpectedVersion ([string]$Journal.expected_version) `
        -ReleaseTag ([string]$Journal.release_tag)
    Write-Success 'Recovered the durable receipt for a committed Windows self-update'
}

function Restore-InstallTransaction {
    if (-not $script:InstallTransactionActive) {
        Exit-InstallLock
        return @()
    }
    $script:InstallTransactionActive = $false
    $restoreFailures = New-Object 'System.Collections.Generic.List[string]'

    if (Test-Path -LiteralPath $tmpFile) {
        try {
            Remove-Item -LiteralPath $tmpFile -Force -ErrorAction Stop
        } catch {
            [void]$restoreFailures.Add("could not remove staged binary at $tmpFile")
        }
    }
    Remove-Item -LiteralPath $stagingDir -Force -ErrorAction SilentlyContinue

    if (($script:GitShimPublishAttempted -or (-not $script:GitShimWasPresent)) -and (Test-Path -LiteralPath $gitShim)) {
        try {
            Remove-Item -LiteralPath $gitShim -Force -ErrorAction Stop
        } catch {
            [void]$restoreFailures.Add("could not remove failed git shim at $gitShim")
        }
    }
    if ($script:GitShimPreserved) {
        try {
            if (Test-Path -LiteralPath $gitShim) {
                Remove-Item -LiteralPath $gitShim -Force -ErrorAction Stop
            }
            Move-Item -LiteralPath $gitShimBackup -Destination $gitShim -ErrorAction Stop
        } catch {
            [void]$restoreFailures.Add("recover the previous git shim from $gitShimBackup")
        }
    }

    if ($script:BinaryPublishAttempted -and (Test-Path -LiteralPath $finalExe)) {
        try {
            Remove-Item -LiteralPath $finalExe -Force -ErrorAction Stop
        } catch {
            [void]$restoreFailures.Add("could not remove failed binary at $finalExe")
        }
    }
    if ($script:BinaryPreserved) {
        try {
            if (Test-Path -LiteralPath $finalExe) {
                Remove-Item -LiteralPath $finalExe -Force -ErrorAction Stop
            }
            Move-Item -LiteralPath $binaryBackup -Destination $finalExe -ErrorAction Stop
        } catch {
            [void]$restoreFailures.Add("recover the previous git-ai binary from $binaryBackup")
        }
    }

    if ($restoreFailures.Count -eq 0) {
        Remove-Item -LiteralPath $installJournal -Force -ErrorAction SilentlyContinue
    }
    Exit-InstallLock

    if ($script:InstallDirCreated) {
        try { Remove-Item -LiteralPath $installDir -ErrorAction Stop } catch { }
    }
    if ($script:InstallRootCreated) {
        try { Remove-Item -LiteralPath $installRoot -ErrorAction Stop } catch { }
    }

    return $restoreFailures
}

function Complete-InstallTransaction {
    param(
        [Parameter(Mandatory = $true)][string]$InstalledVersion,
        [Parameter(Mandatory = $false)][AllowEmptyString()][string]$ExpectedVersion
    )

    Write-InstallJournal -Phase 'committed'
    Invoke-InstallCrashIfRequested -Step 'after_committed_journal_before_receipt'
    # Keep the installer lock until the detached-upgrade receipt is durable.
    # Otherwise another installer can publish a different binary between this
    # transaction and the receipt, leaving a valid receipt for the wrong file.
    Write-UpgradeReceiptIfRequested -InstalledVersion $InstalledVersion -ExpectedVersion $ExpectedVersion
    $script:InstallTransactionActive = $false
    $cleanupFailed = $false
    foreach ($backup in @($binaryBackup, $gitShimBackup)) {
        if (Test-Path -LiteralPath $backup) {
            try {
                Remove-Item -LiteralPath $backup -Force -ErrorAction Stop
            } catch {
                Write-Warning "Installed successfully, but could not remove obsolete backup: $backup"
                $cleanupFailed = $true
            }
        }
    }
    Remove-StagedCandidate
    if (-not $cleanupFailed) {
        Remove-Item -LiteralPath $installJournal -Force -ErrorAction SilentlyContinue
    } else {
        Write-Warning "Committed recovery journal retained for cleanup on the next installer run: $installJournal"
    }
    Exit-InstallLock
}

function Invoke-InstallFailureIfRequested {
    param(
        [Parameter(Mandatory = $true)][string]$Step
    )
    if ($env:GIT_AI_INSTALL_TEST_FAIL_AT -eq $Step) {
        throw [System.InvalidOperationException]::new("Injected installer failure at $Step")
    }
}

function Invoke-InstallCrashIfRequested {
    param(
        [Parameter(Mandatory = $true)][string]$Step
    )
    if ($env:GIT_AI_INSTALL_TEST_CRASH_AT -eq $Step) {
        [Environment]::Exit(86)
    }
}

$script:InstallTransactionActive = $false
try {
    Enter-InstallLock
    Recover-InterruptedInstall
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    $script:InstallTransactionActive = $true
    Initialize-StagingDirectory

    Write-Host ("Downloading git-ai (release: {0})..." -f $releaseTag)

function Try-Download {
    param(
        [Parameter(Mandatory = $true)][string]$Url
    )
    try {
        # Disable progress bar to avoid extreme slowdown caused by PowerShell's
        # progress-stream rendering (can make downloads 10-50x slower).
        $oldProgressPreference = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        try {
            Invoke-WebRequest -Uri $Url -OutFile $tmpFile -UseBasicParsing -ErrorAction Stop
        } finally {
            $ProgressPreference = $oldProgressPreference
        }
        return $true
    } catch {
        return $false
    }
}

# Track which download URL succeeded for checksum verification
$downloadedBinaryName = $null
if (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_LOCAL_BINARY)) {
    if (-not (Test-Path -LiteralPath $env:GIT_AI_LOCAL_BINARY)) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit "Local binary not found at $($env:GIT_AI_LOCAL_BINARY)"
    }
    Copy-Item -Force -Path $env:GIT_AI_LOCAL_BINARY -Destination $tmpFile
    $downloadedBinaryName = "$binaryName.exe"
} elseif (Try-Download -Url $downloadUrlExe) {
    $downloadedBinaryName = "$binaryName.exe"
} elseif (Try-Download -Url $downloadUrlNoExt) {
    $downloadedBinaryName = $binaryName
}

if (-not $downloadedBinaryName) {
    Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
    Write-ErrorAndExit 'Failed to download binary (HTTP error)'
}

try {
    if ((Get-Item $tmpFile).Length -le 0) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit 'Downloaded file is empty'
    }
} catch {
    Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
    Write-ErrorAndExit 'Download failed'
}

# Verify checksum if embedded (release builds only)
Verify-Checksum -File $tmpFile -BinaryName $downloadedBinaryName

try { Unblock-File -Path $tmpFile -ErrorAction SilentlyContinue } catch { }
try {
    $candidateVersionOutput = ((& $tmpFile --version 2>&1) | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        Write-ErrorAndExit 'Downloaded binary failed version validation'
    }
} catch {
    Write-ErrorAndExit "Downloaded binary failed version validation: $($_.Exception.Message)"
}
$candidateVersion = Get-NormalizedVersion -Text $candidateVersionOutput
if ([string]::IsNullOrWhiteSpace($candidateVersion)) {
    Write-ErrorAndExit "Downloaded binary returned an unrecognized version: $candidateVersionOutput"
}

$expectedVersionSource = $env:GIT_AI_INSTALL_EXPECTED_VERSION
if ([string]::IsNullOrWhiteSpace($expectedVersionSource) -and
    $PinnedVersion -ne '__VERSION_PLACEHOLDER__' -and $PinnedVersion -ne 'latest') {
    $expectedVersionSource = $PinnedVersion
}
$expectedVersion = $null
if (-not [string]::IsNullOrWhiteSpace($expectedVersionSource)) {
    $expectedVersion = Get-NormalizedVersion -Text $expectedVersionSource
    if ([string]::IsNullOrWhiteSpace($expectedVersion)) {
        Write-ErrorAndExit "Expected release version is invalid: $expectedVersionSource"
    }
    if ($candidateVersion -ne $expectedVersion) {
        Write-ErrorAndExit "Downloaded binary version mismatch: expected $expectedVersion, got $candidateVersion"
    }
}
$script:ExpectedInstallVersion = $expectedVersion
if ($script:UpgradeReceiptRequested) {
    if ([string]::IsNullOrWhiteSpace($expectedVersion)) {
        Write-ErrorAndExit 'Windows self-update requires an exact expected version before publishing files'
    }
    $releaseVersion = Get-NormalizedVersion -Text $releaseTag
    if ($releaseVersion -ne $expectedVersion) {
        Write-ErrorAndExit "Windows self-update release tag $releaseTag does not match expected version $expectedVersion"
    }
}

# Wait for every existing executable to become available before moving any of
# them. This avoids stopping halfway with a mixed binary/shim version.
if (Test-Path -LiteralPath $finalExe) {
    if (-not (Wait-ForFileAvailable -Path $finalExe -InstallDir $installDir -MaxWaitSeconds 300 -RetryIntervalSeconds 5)) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit "Timeout waiting for $finalExe to be available. Please close any running git-ai processes and try again."
    }
}

$script:GitShimWasPresent = Test-Path -LiteralPath $gitShim
if ($script:GitShimWasPresent) {
    if (-not (Wait-ForFileAvailable -Path $gitShim -InstallDir $installDir -MaxWaitSeconds 300 -RetryIntervalSeconds 5)) {
        Write-ErrorAndExit "Timeout waiting for $gitShim to be available. Please close any running git processes and try again."
    }
}

foreach ($managedPath in @($finalExe, $gitShim)) {
    if (Test-Path -LiteralPath $managedPath) {
        $managedItem = Get-Item -LiteralPath $managedPath -Force
        if ($managedItem.PSIsContainer) {
            Write-ErrorAndExit "Managed executable path is a directory: $managedPath"
        }
    }
}
foreach ($backup in @($binaryBackup, $gitShimBackup)) {
    if (Test-Path -LiteralPath $backup) {
        Write-ErrorAndExit "Installer backup path already exists: $backup"
    }
}

$script:BinaryWasPresent = Test-Path -LiteralPath $finalExe
$script:GitShimWasPresent = Test-Path -LiteralPath $gitShim
if ($script:BinaryWasPresent) {
    try {
        $previousVersionOutput = ((& $finalExe --version 2>&1) | Out-String).Trim()
        if ($LASTEXITCODE -eq 0) {
            $script:PreviousVersion = Get-NormalizedVersion -Text $previousVersionOutput
        }
    } catch {
        $script:PreviousVersion = $null
    }
}
if (-not [string]::IsNullOrWhiteSpace($script:PreviousVersion) -and
    (Compare-NumericVersion -Left $script:PreviousVersion -Right $candidateVersion) -gt 0 -and
    $env:GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE -ne '1') {
    Write-ErrorAndExit (
        "Refusing downgrade from $($script:PreviousVersion) to $candidateVersion because local database schemas are forward-only. " +
        'Back up %USERPROFILE%\.git-ai and validate schema compatibility before retrying with GIT_AI_ALLOW_SCHEMA_UNSAFE_DOWNGRADE=1.'
    )
}

Write-InstallJournal -Phase 'prepared'

# Preserve both old entry points before publishing the candidate.
if (Test-Path -LiteralPath $finalExe) {
    Move-Item -LiteralPath $finalExe -Destination $binaryBackup -ErrorAction Stop
    $script:BinaryPreserved = $true
}
if ($script:GitShimWasPresent) {
    Move-Item -LiteralPath $gitShim -Destination $gitShimBackup -ErrorAction Stop
    $script:GitShimPreserved = $true
}

Invoke-InstallFailureIfRequested -Step 'after_backups_preserved'

$script:BinaryPublishAttempted = $true
Move-Item -LiteralPath $tmpFile -Destination $finalExe -ErrorAction Stop
try { Unblock-File -Path $finalExe -ErrorAction SilentlyContinue } catch { }

$installedVersionOutput = ((& $finalExe --version 2>&1) | Out-String).Trim()
if ($LASTEXITCODE -ne 0) { Write-ErrorAndExit 'Installed binary failed version validation' }
$installedVersion = Get-NormalizedVersion -Text $installedVersionOutput
if ([string]::IsNullOrWhiteSpace($installedVersion) -or $installedVersion -ne $candidateVersion) {
    Write-ErrorAndExit "Published binary version changed during installation: expected $candidateVersion, got $installedVersion"
}
if (-not [string]::IsNullOrWhiteSpace($expectedVersion) -and $installedVersion -ne $expectedVersion) {
    Write-ErrorAndExit "Installed binary version mismatch: expected $expectedVersion, got $installedVersion"
}
Write-Host $installedVersionOutput
Invoke-InstallFailureIfRequested -Step 'after_binary_publish'

# Refresh git.exe only for existing wrapper users (it is a copy, not a
# symlink, on Windows). A first install must not invent this legacy shim.
if ($script:GitShimWasPresent) {
    $script:GitShimPublishAttempted = $true
    Copy-Item -LiteralPath $finalExe -Destination $gitShim -ErrorAction Stop
    try { Unblock-File -Path $gitShim -ErrorAction SilentlyContinue } catch { }
}
Invoke-InstallFailureIfRequested -Step 'after_shim_publish'

# Login user with install token if provided
$needLogin = $false
if ($env:INSTALL_NONCE -and $env:API_BASE) {
    try {
        & $finalExe exchange-nonce | Out-Host
        if ($LASTEXITCODE -ne 0) {
            $needLogin = $true
        }
    } catch {
        $needLogin = $true
    }
}

if ($needLogin) {
    Write-Host ''
    Write-Host 'Launching login...'
    & $finalExe login | Out-Host
    if ($LASTEXITCODE -ne 0) {
        Write-ErrorAndExit 'Login failed'
    }
}

# Install hooks. --env also updates the persistent user PATH and configures
# Git Bash shell profiles.
Write-Host 'Setting up IDE/agent hooks...'
try {
    & $finalExe install-hooks --env | Out-Host
    if ($LASTEXITCODE -eq 0) {
        Write-Success 'Successfully set up IDE/agent hooks'
    } else {
        Write-Warning "Warning: Failed to set up IDE/agent hooks. Please try running 'git-ai install-hooks' manually."
    }
} catch {
    Write-Warning "Warning: Failed to set up IDE/agent hooks. Please try running 'git-ai install-hooks' manually."
}

# Update the current session PATH here: `install-hooks --env` handles the
# persistent user PATH, but a child process cannot modify this session.
if ($env:GIT_AI_SKIP_PATH_UPDATE -ne '1') {
    try {
        $normalizedAdd = Normalize-PathString $installDir
        $procEntries = @()
        if ($env:PATH) { $procEntries = ($env:PATH -split ';') | Where-Object { $_ -and $_.Trim() -ne '' } }
        $procHas = $false
        foreach ($e in $procEntries) {
            if ((Normalize-PathString $e) -eq $normalizedAdd) { $procHas = $true; break }
        }
        if (-not $procHas) {
            $env:PATH = if ($env:PATH) { "$($env:PATH);$installDir" } else { $installDir }
        }
    } catch { }
}

# A detached Windows self-update is not successful until the installed binary
# exactly matches the expected release and its durable receipt exists. Both are
# committed while the installer lock is still held.
Complete-InstallTransaction -InstalledVersion $installedVersion -ExpectedVersion $expectedVersion

# Best-effort restart only after the executable transaction is committed.
Start-DaemonIfRequested

Write-Success "Successfully installed git-ai into $installDir"
Write-Success "You can now run 'git-ai' from your terminal"
Write-Host 'Close and reopen your terminal and IDE sessions to use git-ai.' -ForegroundColor Yellow
} catch {
    $failureMessage = $_.Exception.Message
    $restoreFailures = @(Restore-InstallTransaction)
    Write-Host "Error: $failureMessage" -ForegroundColor Red
    if ($restoreFailures.Count -gt 0) {
        Write-Host 'The previous executable set could not be restored completely:' -ForegroundColor Red
        foreach ($restoreFailure in $restoreFailures) {
            Write-Host "  - $restoreFailure" -ForegroundColor Red
        }
        Write-Host "Configuration, SQLite, and outbox data under $installRoot were not removed." -ForegroundColor Yellow
    }
    Start-DaemonIfRequested
    exit 1
}
