@echo off
setlocal
cd /d "%~dp0.."

cargo build -r
if errorlevel 1 exit /b %errorlevel%

.\target\release\rust-bot agent --config ./configs/anthropic/config_gmail_anthropic.json --logs
