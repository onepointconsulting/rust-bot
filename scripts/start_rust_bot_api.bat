@echo off
setlocal
cd /d "%~dp0.."

cargo build -r
if errorlevel 1 exit /b %errorlevel%

.\target\release\rust-bot api --config ./configs/openai-compat/config.json
