param(
    [Parameter(Mandatory = $true)][string]$BinaryPath,
    [Parameter(Mandatory = $false)][string]$InstallerPath = (Join-Path $PSScriptRoot '../../install.ps1')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$binary = (Resolve-Path -LiteralPath $BinaryPath).Path
$installer = (Resolve-Path -LiteralPath $InstallerPath).Path

$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$errors)
if ($errors.Count -gt 0) {
    throw ($errors | Out-String)
}

# Load only the recovery helpers. Never execute the installer's top-level actions.
foreach ($name in @(
    'Write-Success',
    'Write-Warning',
    'Normalize-PathString',
    'Test-FileAvailable',
    'Stop-GitAiBackgroundService',
    'Get-GitAiManagedProcesses',
    'Stop-GitAiManagedProcesses',
    'Wait-ForFileAvailable',
    'Remove-StagedCandidate',
    'Restore-RecoveredPath',
    'Recover-InterruptedInstall'
)) {
    $function = $ast.Find({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $false)
    if ($null -eq $function) {
        throw "Missing installer function: $name"
    }
    . ([scriptblock]::Create($function.Extent.Text))
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('git-ai-win-recovery-' + [guid]::NewGuid().ToString('N'))
$testHome = Join-Path $testRoot 'home'
$installRoot = Join-Path $testHome '.git-ai'
$installDir = Join-Path $installRoot 'bin'
$finalExe = Join-Path $installDir 'git-ai.exe'
$gitShim = Join-Path $installDir 'git.exe'
$stagingDir = Join-Path $installDir '.git-ai.install-staged'
$tmpFile = Join-Path $stagingDir 'git-ai.exe'
$binaryBackup = "$finalExe.install-backup"
$gitShimBackup = "$gitShim.install-backup"
$installJournal = Join-Path $installRoot 'install-transaction.json'

$daemonProcessIds = @()
$environmentNames = @('HOME', 'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH', 'GIT_AI_ALLOW_SUPERUSER')
$savedEnvironment = @{}
foreach ($name in $environmentNames) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

function Get-TestManagedProcesses {
    if (-not (Test-Path -LiteralPath $installDir)) {
        return @()
    }
    $target = Normalize-PathString $finalExe
    return @(
        Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object {
            $_.ExecutablePath -and (Normalize-PathString $_.ExecutablePath) -eq $target
        }
    )
}

try {
    New-Item -ItemType Directory -Force -Path $stagingDir | Out-Null

    $homeDrive = [IO.Path]::GetPathRoot($testHome).TrimEnd('\')
    $homePath = $testHome.Substring($homeDrive.Length)
    $env:HOME = $testHome
    $env:USERPROFILE = $testHome
    $env:HOMEDRIVE = $homeDrive
    $env:HOMEPATH = $homePath
    $env:GIT_AI_ALLOW_SUPERUSER = '1'

    Copy-Item -LiteralPath $binary -Destination $finalExe -Force
    Copy-Item -LiteralPath $binary -Destination $tmpFile -Force

    $versionOutput = ((& $finalExe --version 2>&1) | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "Fixture binary failed --version: $versionOutput"
    }
    $versionMatch = [regex]::Match($versionOutput, '(?<![0-9])([0-9]+\.[0-9]+\.[0-9]+(?:\.[0-9]+)*)(?![0-9])')
    if (-not $versionMatch.Success) {
        throw "Could not parse fixture binary version: $versionOutput"
    }
    $version = $versionMatch.Groups[1].Value

    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    $oldMarker = 'old binary marker' + [Environment]::NewLine
    [IO.File]::WriteAllText($binaryBackup, $oldMarker, $utf8NoBom)

    $journal = [ordered]@{
        format = 2
        phase = 'prepared'
        binary_was_present = $true
        git_shim_was_present = $false
        upgrade_receipt_requested = $false
        upgrade_receipt_path = ''
        expected_version = $version
        release_tag = ''
    }
    [IO.File]::WriteAllText($installJournal, ($journal | ConvertTo-Json -Compress), $utf8NoBom)

    $startOutput = ((& $finalExe bg start 2>&1) | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "Could not start fixture daemon: $startOutput"
    }

    $daemonProcesses = @()
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        $daemonProcesses = @(Get-TestManagedProcesses)
        if ($daemonProcesses.Count -gt 0) {
            break
        }
        Start-Sleep -Milliseconds 250
    }
    if ($daemonProcesses.Count -eq 0) {
        throw 'Fixture daemon did not remain running from the installed git-ai.exe path'
    }
    $daemonProcessIds = @($daemonProcesses | Select-Object -ExpandProperty ProcessId)

    if (Test-FileAvailable -Path $finalExe) {
        throw 'Fixture failed: the running daemon did not lock git-ai.exe for exclusive write'
    }

    Recover-InterruptedInstall

    if (Test-Path -LiteralPath $binaryBackup) {
        throw 'Recovery left git-ai.exe.install-backup behind'
    }
    if (Test-Path -LiteralPath $installJournal) {
        throw 'Recovery left install-transaction.json behind'
    }
    if (Test-Path -LiteralPath $stagingDir) {
        throw 'Recovery left the staging directory behind'
    }
    if (-not (Test-Path -LiteralPath $finalExe)) {
        throw 'Recovery did not restore the previous git-ai path'
    }
    if ([IO.File]::ReadAllText($finalExe) -ne $oldMarker) {
        throw 'Recovery did not restore the previous git-ai bytes'
    }

    foreach ($daemonProcessId in $daemonProcessIds) {
        if (Get-Process -Id $daemonProcessId -ErrorAction SilentlyContinue) {
            throw "Recovery left daemon PID $daemonProcessId running"
        }
    }

    Write-Host 'Windows interrupted-install locked executable recovery test passed'
}
finally {
    foreach ($daemonProcessId in $daemonProcessIds) {
        Stop-Process -Id $daemonProcessId -Force -ErrorAction SilentlyContinue
    }
    foreach ($process in @(Get-TestManagedProcesses)) {
        Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
    }
    Start-Sleep -Milliseconds 200
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
    }

    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
}
