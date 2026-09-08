# Runs on PowerShell 5.1+ without installing Git AI or invoking Windows APIs.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$installer = Join-Path $PSScriptRoot '../../install.ps1'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
# Load only pure validation functions, never the installer's top-level actions.
foreach ($name in @('Read-OfflineBundle', 'Verify-Checksum', 'Write-ErrorAndExit', 'Write-Success', 'Get-NormalizedVersion')) {
    $function = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $false)
    if ($null -eq $function) { throw "Missing function: $name" }
    . ([scriptblock]::Create($function.Extent.Text))
}
$script:InstallTransactionActive = $true
$source = [IO.File]::ReadAllText($installer)
$versionStart = $source.IndexOf('$expectedVersionSource = $env:GIT_AI_INSTALL_EXPECTED_VERSION')
$versionEnd = $source.IndexOf('$script:ExpectedInstallVersion = $expectedVersion')
if ($versionStart -lt 0 -or $versionEnd -le $versionStart) { throw 'Expected version gate missing' }
$versionGate = [scriptblock]::Create($source.Substring($versionStart, $versionEnd - $versionStart))
$savedExpected = $env:GIT_AI_INSTALL_EXPECTED_VERSION
$savedLocal = $env:GIT_AI_LOCAL_BINARY
$selectionStart = $source.IndexOf('$localBinaryPath = $env:GIT_AI_LOCAL_BINARY')
$selectionEnd = $source.IndexOf('# Determine release tag', $selectionStart)
if ($selectionStart -lt 0 -or $selectionEnd -le $selectionStart) { throw 'Local source selection missing' }
# Extracted blocks have no file context; explicitly supply the installer's directory.
$selectionGate = [scriptblock]::Create($source.Substring($selectionStart, $selectionEnd - $selectionStart).Replace('$PSScriptRoot', '$installerDirectory'))
function Select-Source([string]$installerDirectory) {
    $binaryName = 'git-ai-windows-x64'
    . $selectionGate
    return [pscustomobject]@{ Path = $localBinaryPath; Bundle = $offlineBundle }
}
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('git-ai-manifest-' + [guid]::NewGuid())
$utf8 = New-Object Text.UTF8Encoding($false)
function Write-Fixture([string]$Version) {
    $script:bundle = Join-Path $testRoot ('bundle [offline] ' + $Version)
    [void][IO.Directory]::CreateDirectory((Join-Path $bundle 'windows'))
    $script:binary = Join-Path $bundle 'windows/git-ai-windows-x64.exe'
    [IO.File]::WriteAllText($binary, "binary $Version", $utf8)
    $script:metadata = Join-Path $bundle 'BUILD-METADATA.txt'
    [IO.File]::WriteAllText($metadata, "bundle_format=2`ncli_version=$Version`n", $utf8)
    Write-Checksums
}
function Write-Checksums {
    $script:manifest = Join-Path $bundle 'SHA256SUMS'
    $binaryHash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()
    $metadataHash = (Get-FileHash -LiteralPath $metadata -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText($manifest, "$binaryHash  windows/git-ai-windows-x64.exe`n$metadataHash  BUILD-METADATA.txt`n", $utf8)
}
function Expect-Rejected([scriptblock]$Action, [string]$Label) {
    $rejected = $false
    try { & $Action | Out-Null } catch { $rejected = $true }
    if (-not $rejected) { throw "Unexpectedly accepted: $Label" }
}
try {
    # The same loaded installer must accept successive manifests, even from a different CWD.
    foreach ($version in @('1.6.17', '1.6.18')) {
        Write-Fixture $version
        $env:GIT_AI_LOCAL_BINARY = $binary
        $selected = Select-Source $testRoot
        if ($selected.Bundle.Version -ne $version) { throw 'Fixed-location installer did not select new bundle' }
        $env:GIT_AI_LOCAL_BINARY = $null
        $selected = Select-Source $bundle
        if ($selected.Bundle.Version -ne $version) { throw 'Bundle auto-detection failed' }
        $result = Read-OfflineBundle -BinaryPath $binary -BinaryName 'git-ai-windows-x64.exe'
        if ($result.Version -ne $version) { throw 'Installer retained an old version' }
        if ($result.Checksums -notmatch '^[0-9a-f]{64}  git-ai-windows-x64.exe$') { throw 'Wrong checksum selected' }
        $EmbeddedChecksums = $result.Checksums
        $staged = Join-Path $testRoot 'git-ai.exe'
        Copy-Item -LiteralPath $binary -Destination $staged -Force
        Verify-Checksum -File $staged -BinaryName 'git-ai-windows-x64.exe'
        $offlineBundle = $result
        $candidateVersion = $version
        $PinnedVersion = 'v1.0.0'
        $env:GIT_AI_INSTALL_EXPECTED_VERSION = $null
        . $versionGate
        if ($expectedVersion -ne $version) { throw 'Offline version did not override pinned version' }
    }
    $candidateVersion = '1.6.17'
    Expect-Rejected { . $versionGate } 'binary version mismatch'
    $candidateVersion = '1.6.18'
    $env:GIT_AI_INSTALL_EXPECTED_VERSION = '1.6.19'
    Expect-Rejected { . $versionGate } 'explicit version conflict'
    $env:GIT_AI_INSTALL_EXPECTED_VERSION = $null
    $offlineBundle = $null
    $PinnedVersion = 'v1.6.18'
    . $versionGate
    if ($expectedVersion -ne '1.6.18') { throw 'Online pinned version lost' }
    $PinnedVersion = '__VERSION_PLACEHOLDER__'
    . $versionGate
    if ($null -ne $expectedVersion) { throw 'Development binary now requires a manifest' }
    [IO.File]::AppendAllText($staged, 'tampered', $utf8)
    Expect-Rejected { Verify-Checksum -File $staged -BinaryName 'git-ai-windows-x64.exe' } 'modified staged binary'
    if (-not (Test-Path -LiteralPath $binary)) { throw 'Source binary removed by verification' }
    $env:GIT_AI_LOCAL_BINARY = $null
    $selected = Select-Source ''
    if ($null -ne $selected.Bundle -or -not [string]::IsNullOrWhiteSpace($selected.Path)) {
        throw 'Online iex entry requires a script directory'
    }
    [IO.File]::WriteAllText((Join-Path $testRoot 'SHA256SUMS'), 'online release checksums', $utf8)
    $selected = Select-Source $testRoot
    if ($null -ne $selected.Bundle -or -not [string]::IsNullOrWhiteSpace($selected.Path)) {
        throw 'Online release incorrectly treated as an offline bundle'
    }
    $developmentBinary = Join-Path $testRoot 'git-ai.exe'
    [IO.File]::WriteAllText($developmentBinary, 'development binary', $utf8)
    $env:GIT_AI_LOCAL_BINARY = $developmentBinary
    $selected = Select-Source $testRoot
    if ($null -ne $selected.Bundle -or $selected.Path -ne $developmentBinary) { throw 'Bare development source rejected' }
    [IO.File]::AppendAllText($metadata, 'tampered', $utf8)
    Expect-Rejected { Read-OfflineBundle $binary 'git-ai-windows-x64.exe' } 'modified metadata'
    Write-Fixture '1.6.18'
    [IO.File]::AppendAllText($manifest, [IO.File]::ReadAllText($manifest), $utf8)
    Expect-Rejected { Read-OfflineBundle $binary 'git-ai-windows-x64.exe' } 'duplicate checksum entries'
    Write-Fixture '1.6.18'
    [IO.File]::AppendAllText($metadata, "cli_version=1.6.19`n", $utf8)
    Write-Checksums
    Expect-Rejected { Read-OfflineBundle $binary 'git-ai-windows-x64.exe' } 'duplicate version'
    Write-Fixture 'garbage'
    Expect-Rejected { Read-OfflineBundle $binary 'git-ai-windows-x64.exe' } 'invalid version'
    Write-Fixture '1.6.18'
    Expect-Rejected { Read-OfflineBundle $binary 'git-ai-windows-arm64.exe' } 'wrong architecture'
    [IO.File]::Delete($manifest)
    Expect-Rejected { Read-OfflineBundle $binary 'git-ai-windows-x64.exe' } 'missing manifest'
    $env:GIT_AI_LOCAL_BINARY = $null
    Expect-Rejected { Select-Source $bundle } 'incomplete bundle auto-detection'
    Write-Fixture '1.6.18'
    [IO.File]::WriteAllText($manifest, 'invalid checksum line', $utf8)
    Expect-Rejected { Read-OfflineBundle $binary 'git-ai-windows-x64.exe' } 'malformed manifest'
    Write-Host 'Windows offline manifest tests passed'
} finally {
    $env:GIT_AI_INSTALL_EXPECTED_VERSION = $savedExpected
    $env:GIT_AI_LOCAL_BINARY = $savedLocal
    if (Test-Path -LiteralPath $testRoot) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
}
