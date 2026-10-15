<#
.SYNOPSIS
  One-line installer for rust-bot (Windows).

.DESCRIPTION
  irm https://raw.githubusercontent.com/onepointconsulting/rust-bot/main/scripts/install.ps1 | iex

  Downloads the latest GitHub release, installs it under %LOCALAPPDATA%\rust-bot,
  adds that folder to the user PATH and runs `rust-bot onboard` from the user profile
  so the config lands in ~\.rust-bot\. Set $env:RUST_BOT_VERSION (e.g. 0.3.3) to pin a version.
#>
$ErrorActionPreference = "Stop"
$repo = "onepointconsulting/rust-bot"
$installDir = Join-Path $env:LOCALAPPDATA "rust-bot"

# Returns the version to install: $env:RUST_BOT_VERSION or the latest release tag without the "v".
function Get-RustBotVersion {
    if ($env:RUST_BOT_VERSION) { return $env:RUST_BOT_VERSION.TrimStart("v") }
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
    return $release.tag_name.TrimStart("v")
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
        if (Test-Path $installDir) { Remove-Item $installDir -Recurse -Force }
        Move-Item (Join-Path $workDir $stageName) $installDir
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

$version = Get-RustBotVersion
Install-RustBotRelease $version
Add-InstallDirToPath

Write-Host "Installed rust-bot $version. Starting onboarding..."
Set-Location $env:USERPROFILE
& (Join-Path $installDir "rust-bot.exe") onboard
