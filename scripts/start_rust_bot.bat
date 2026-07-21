@echo off
setlocal
rem Repo root = parent of scripts/ (works from any cwd when invoking this file)
cd /d "%~dp0.."

cargo build
if errorlevel 1 exit /b %errorlevel%

set "SESSION_ARGS="

if /I "%~1"=="--session" (
    if not "%~2"=="" (
        set "SESSION_ARGS=--session %~2"
    )
)

.\target\debug\rust-bot agent --config ./configs/openai-compat/config.json %SESSION_ARGS%
