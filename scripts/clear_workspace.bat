@echo off
setlocal
cd /d "%~dp0.."

rem Clears agent workspace files under .\workspace (repo root)
if not exist "workspace\" (
    echo No workspace\ directory found under %CD%
    exit /b 1
)

pushd workspace
del /q SOUL.md TOOLS.md USER.md HEARTBEAT.md AGENTS.md 2>nul
if exist memory rmdir /s /q memory
if exist sessions rmdir /s /q sessions
popd

echo Cleared workspace\ under %CD%
