<#
.SYNOPSIS
  Uninstaller for rust-bot (Windows), the counterpart of install.ps1.

.DESCRIPTION
  irm https://raw.githubusercontent.com/onepointconsulting/rust-bot/main/scripts/uninstall.ps1 | iex

  Removes %LOCALAPPDATA%\rust-bot and its entry in the user PATH. The config and workspace in
  ~\.rust-bot\ are kept unless you confirm their deletion when asked
  ($env:RUST_BOT_PURGE = "yes" or "no" answers for you). Stop any running rust-bot first,
  Windows locks the files of a running program.
  Set $env:RUST_BOT_SOURCE_ONLY = "1" to load the functions without uninstalling (used by the tests).
#>
$ErrorActionPreference = "Stop"
# These defaults must stay in sync with install.ps1.
$installDir = if ($env:RUST_BOT_INSTALL_DIR) { $env:RUST_BOT_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "rust-bot" }
$dataDir = if ($env:RUST_BOT_DATA_DIR) { $env:RUST_BOT_DATA_DIR } else { Join-Path $env:USERPROFILE ".rust-bot" }

# Throws when a rust-bot.exe from $installDir is running, because Windows cannot delete it then.
function Assert-RustBotNotRunning {
    $exePath = Join-Path $installDir "rust-bot.exe"
    $running = Get-Process -Name "rust-bot" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exePath }
    if ($running) {
        throw "rust-bot is running (PID $($running.Id -join ', ')). Stop it (for example Ctrl+C in its terminal) and run this script again."
    }
}

# Returns $PathValue without the entry $Entry (case-insensitive, trailing backslash ignored).
function Remove-PathEntry([string]$PathValue, [string]$Entry) {
    $normalizedEntry = $Entry.TrimEnd("\")
    $kept = $PathValue -split ";" | Where-Object { $_ -and $_.TrimEnd("\") -ine $normalizedEntry }
    return ($kept -join ";")
}

# Removes $installDir from the persisted user PATH and from the current session.
function Remove-InstallDirFromPath {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $cleanedUserPath = if ($userPath) { Remove-PathEntry $userPath $installDir } else { $userPath }
    # Only write when something changed: rewriting the value flattens %VARIABLES% in it.
    if ($cleanedUserPath -ne $userPath) {
        [Environment]::SetEnvironmentVariable("Path", $cleanedUserPath, "User")
    }
    $env:Path = Remove-PathEntry $env:Path $installDir
}

# Returns $true when the config/workspace in $dataDir should be deleted.
# $env:RUST_BOT_PURGE = yes|no decides without asking, otherwise the user is asked.
function Confirm-PurgeData {
    switch -Regex ($env:RUST_BOT_PURGE) {
        "^(yes|y|1|true)$" { return $true }
        "^(no|n|0|false)$" { return $false }
    }
    $answer = Read-Host "Also delete your config, API keys and workspace in $dataDir? [y/N]"
    return ($answer -match "^(y|yes)$")
}

# Uninstalls rust-bot; the user data is only deleted after confirmation.
function Invoke-RustBotUninstall {
    Assert-RustBotNotRunning
    if (Test-Path $installDir) {
        Remove-Item $installDir -Recurse -Force
        Write-Host "Removed $installDir"
    }
    Remove-InstallDirFromPath
    if (Test-Path $dataDir) {
        if (Confirm-PurgeData) {
            Remove-Item $dataDir -Recurse -Force
            Write-Host "Removed $dataDir"
        } else {
            Write-Host "Kept $dataDir (delete it manually, or set `$env:RUST_BOT_PURGE = 'yes', to remove your data)."
        }
    }
    Write-Host "rust-bot was uninstalled. Open a new terminal for the PATH change to apply."
}

if (-not $env:RUST_BOT_SOURCE_ONLY) { Invoke-RustBotUninstall }
