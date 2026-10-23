<#
.SYNOPSIS
  One-line installer for rust-bot (Windows).

.DESCRIPTION
  irm https://raw.githubusercontent.com/onepointconsulting/rust-bot/main/scripts/install.ps1 | iex

  Downloads the latest GitHub release, installs it under %LOCALAPPDATA%\rust-bot,
  adds that folder to the user PATH and runs `rust-bot onboard` from the user profile
  so the config lands in ~\.rust-bot\. Set $env:RUST_BOT_VERSION (e.g. 0.3.3) to pin a version.

  Re-running the script on an existing install updates it in place: the config in ~\.rust-bot\
  is untouched, onboarding is skipped, and nothing happens when the requested version is already
  installed (set $env:RUST_BOT_FORCE = "1" to reinstall anyway). Stop any running rust-bot first,
  Windows locks the files of a running program.
  Set $env:RUST_BOT_SOURCE_ONLY = "1" to load the functions without installing (used by the tests).
#>
$ErrorActionPreference = "Stop"
$repo = "onepointconsulting/rust-bot"
$installDir = if ($env:RUST_BOT_INSTALL_DIR) { $env:RUST_BOT_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "rust-bot" }

# Returns the version to install: $env:RUST_BOT_VERSION or the latest release tag without the "v".
function Get-RustBotVersion {
    if ($env:RUST_BOT_VERSION) { return $env:RUST_BOT_VERSION.TrimStart("v") }
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
    return $release.tag_name.TrimStart("v")
}

# Returns the version of the rust-bot installed in $installDir, or $null when there is none.
function Get-InstalledRustBotVersion {
    $exePath = Join-Path $installDir "rust-bot.exe"
    if (-not (Test-Path $exePath)) { return $null }
    $versionLine = & $exePath --version 2>$null | Select-Object -First 1
    if (-not $versionLine) { return $null }
    return ($versionLine -split "\s+")[-1]
}

# Throws when a rust-bot.exe from $installDir is running, because Windows cannot replace it then.
function Assert-RustBotNotRunning {
    $exePath = Join-Path $installDir "rust-bot.exe"
    $running = Get-Process -Name "rust-bot" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exePath }
    if ($running) {
        throw "rust-bot is running (PID $($running.Id -join ', ')). Stop it (for example Ctrl+C in its terminal) and run this script again."
    }
}

# Replaces $installDir with the directory $StagedDir. The previous install is kept aside until
# the new one is in place and restored if the move fails, so a failed update never leaves
# the machine without a working rust-bot.
function Switch-InstallDir([string]$StagedDir) {
    $backupDir = "$installDir.old"
    if (Test-Path $backupDir) { Remove-Item $backupDir -Recurse -Force }
    if (Test-Path $installDir) { Move-Item $installDir $backupDir }
    try {
        Move-Item $StagedDir $installDir
    } catch {
        if (Test-Path $backupDir) { Move-Item $backupDir $installDir }
        throw "Could not install into $installDir; the previous install was restored. $_"
    }
    if (Test-Path $backupDir) { Remove-Item $backupDir -Recurse -Force }
}

# Downloads the release zip for $Version and replaces the contents of $installDir.
function Install-RustBotRelease([string]$Version) {
    $stageName = "rust-bot-$Version-windows-x86_64"
    $workDir = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid())
    New-Item -ItemType Directory -Path $workDir | Out-Null
    try {
        Write-Host "Downloading rust-bot $Version (windows-x86_64)..."
        $zipPath = Join-Path $workDir "rust-bot.zip"
        Invoke-WebRequest "https://github.com/$repo/releases/download/v$Version/$stageName.zip" -OutFile $zipPath
        Expand-Archive $zipPath -DestinationPath $workDir
        Switch-InstallDir (Join-Path $workDir $stageName)
    } finally {
        Remove-Item $workDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Adds $installDir to the user PATH (persisted) and to the current session.
function Add-InstallDirToPath {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$installDir", "User")
    }
    $env:Path = "$env:Path;$installDir"
}

# Installs or updates rust-bot to the requested version; onboarding only runs on a fresh install.
function Invoke-RustBotInstall {
    $version = Get-RustBotVersion
    $previousVersion = Get-InstalledRustBotVersion
    if ($previousVersion -eq $version -and -not $env:RUST_BOT_FORCE) {
        Write-Host "rust-bot $version is already installed (set `$env:RUST_BOT_FORCE = '1' to reinstall)."
        return
    }
    Assert-RustBotNotRunning
    Install-RustBotRelease $version
    Add-InstallDirToPath

    if ($previousVersion) {
        Write-Host "Updated rust-bot $previousVersion -> $version. Your config in ~\.rust-bot\ was left untouched."
        return
    }
    Write-Host "Installed rust-bot $version. To remove it later, run: $installDir\uninstall.cmd"
    Write-Host "Starting onboarding..."
    Set-Location $env:USERPROFILE
    & (Join-Path $installDir "rust-bot.exe") onboard
}

if (-not $env:RUST_BOT_SOURCE_ONLY) { Invoke-RustBotInstall }
