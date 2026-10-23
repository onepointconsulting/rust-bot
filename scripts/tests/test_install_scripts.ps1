<#
.SYNOPSIS
  Tests for scripts/install.ps1 and scripts/uninstall.ps1.
  Run: powershell -NoProfile -File scripts\tests\test_install_scripts.ps1

.DESCRIPTION
  Plain PowerShell (no Pester needed). Every test runs against a throw-away directory via the
  RUST_BOT_INSTALL_DIR / RUST_BOT_DATA_DIR overrides, so the real install is never touched.
  The user PATH in the registry is not exercised, only the pure helpers around it.
#>
$ErrorActionPreference = "Stop"
$scriptsDir = Split-Path -Parent $PSScriptRoot
$script:failures = 0
$script:testCount = 0

# Records a failure when $Expected and $Actual differ.
function Assert-Equal($Expected, $Actual, [string]$Description) {
    if ($Expected -ne $Actual) {
        Write-Host "  FAIL: $Description (expected '$Expected', got '$Actual')"
        $script:failures++
    }
}

# Records a failure when the existence of $Path is not $ShouldExist.
function Assert-Exists([string]$Path, [bool]$ShouldExist, [string]$Description) {
    if ((Test-Path $Path) -ne $ShouldExist) {
        Write-Host "  FAIL: $Description ($Path exists: $(-not $ShouldExist))"
        $script:failures++
    }
}

# Creates a directory at $Path holding a marker file, standing in for an installed rust-bot.
function New-FakeInstall([string]$Path, [string]$Marker) {
    New-Item -ItemType Directory -Path $Path -Force | Out-Null
    Set-Content -Path (Join-Path $Path "marker.txt") -Value $Marker
}

# Creates a fresh sandbox and points the scripts at it through their environment overrides.
function Enter-Sandbox {
    $script:sandbox = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid())
    New-Item -ItemType Directory -Path $script:sandbox | Out-Null
    $env:RUST_BOT_INSTALL_DIR = Join-Path $script:sandbox "install"
    $env:RUST_BOT_DATA_DIR = Join-Path $script:sandbox "data"
    $env:RUST_BOT_SOURCE_ONLY = "1"
    $env:RUST_BOT_PURGE = "no"
}

# Runs the test scriptblock $Body after loading $ScriptName, then removes the sandbox.
function Test-Case([string]$Name, [string]$ScriptName, [scriptblock]$Body) {
    $script:testCount++
    Write-Host $Name
    Enter-Sandbox
    # Dot-sourced here (not in Enter-Sandbox) so the script's functions are visible to $Body.
    . (Join-Path $scriptsDir $ScriptName)
    try { & $Body } catch { Write-Host "  FAIL: $_"; $script:failures++ }
    finally { Remove-Item $script:sandbox -Recurse -Force -ErrorAction SilentlyContinue }
}

Test-Case "Get-InstalledRustBotVersion is null without an install" "install.ps1" {
    Assert-Equal $null (Get-InstalledRustBotVersion) "no install means no version"
}

Test-Case "Switch-InstallDir performs a fresh install" "install.ps1" {
    $staged = Join-Path $script:sandbox "staged"
    New-FakeInstall $staged "new"
    Switch-InstallDir $staged
    Assert-Equal "new" ((Get-Content (Join-Path $installDir "marker.txt")) -join "") "fresh install content"
    Assert-Exists "$installDir.old" $false "no backup left"
}

Test-Case "Switch-InstallDir replaces the previous install" "install.ps1" {
    New-FakeInstall $installDir "old"
    $staged = Join-Path $script:sandbox "staged"
    New-FakeInstall $staged "new"
    Switch-InstallDir $staged
    Assert-Equal "new" ((Get-Content (Join-Path $installDir "marker.txt")) -join "") "updated content"
    Assert-Exists "$installDir.old" $false "backup removed after success"
}

Test-Case "Switch-InstallDir restores the previous install on failure" "install.ps1" {
    New-FakeInstall $installDir "old"
    $threw = $false
    try { Switch-InstallDir (Join-Path $script:sandbox "missing") } catch { $threw = $true }
    Assert-Equal $true $threw "failure is reported"
    Assert-Equal "old" ((Get-Content (Join-Path $installDir "marker.txt")) -join "") "previous install restored"
}

Test-Case "Assert-RustBotNotRunning passes when nothing runs from the install dir" "install.ps1" {
    Assert-RustBotNotRunning
}

Test-Case "Remove-PathEntry drops only the matching entry" "uninstall.ps1" {
    $result = Remove-PathEntry "C:\a;C:\Users\x\AppData\Local\rust-bot\;C:\b" "c:\users\x\appdata\local\rust-bot"
    Assert-Equal "C:\a;C:\b" $result "case-insensitive, trailing backslash ignored"
    Assert-Equal "C:\a;C:\b" (Remove-PathEntry "C:\a;C:\b" "C:\other") "unrelated PATH unchanged"
}

Test-Case "Confirm-PurgeData follows RUST_BOT_PURGE" "uninstall.ps1" {
    $env:RUST_BOT_PURGE = "yes"
    Assert-Equal $true (Confirm-PurgeData) "yes purges"
    $env:RUST_BOT_PURGE = "no"
    Assert-Equal $false (Confirm-PurgeData) "no keeps"
}

Test-Case "Uninstall removes the install dir and keeps data by default" "uninstall.ps1" {
    New-FakeInstall $installDir "x"
    New-FakeInstall $dataDir "config"
    $originalUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    try { Invoke-RustBotUninstall | Out-Null }
    finally { [Environment]::SetEnvironmentVariable("Path", $originalUserPath, "User") }
    Assert-Exists $installDir $false "install dir removed"
    Assert-Exists $dataDir $true "data kept"
}

Test-Case "Uninstall purges data when confirmed" "uninstall.ps1" {
    New-FakeInstall $dataDir "config"
    $env:RUST_BOT_PURGE = "yes"
    $originalUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    try { Invoke-RustBotUninstall | Out-Null }
    finally { [Environment]::SetEnvironmentVariable("Path", $originalUserPath, "User") }
    Assert-Exists $dataDir $false "data removed"
}

Test-Case "uninstall.ps1 can delete the install dir it is running from (as bundled in the release)" "uninstall.ps1" {
    New-FakeInstall $installDir "x"
    Copy-Item (Join-Path $scriptsDir "uninstall.ps1") $installDir
    $env:RUST_BOT_SOURCE_ONLY = ""
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $installDir "uninstall.ps1") | Out-Null
    Assert-Equal 0 $LASTEXITCODE "script exits cleanly"
    Assert-Exists $installDir $false "install dir removed while the script ran from it"
}

Remove-Item Env:RUST_BOT_INSTALL_DIR, Env:RUST_BOT_DATA_DIR, Env:RUST_BOT_SOURCE_ONLY, Env:RUST_BOT_PURGE -ErrorAction SilentlyContinue
Write-Host "$script:testCount tests, $script:failures failure(s)"
if ($script:failures -gt 0) { exit 1 }
