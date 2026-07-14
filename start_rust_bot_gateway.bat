@echo off
setlocal

cargo build -r
if errorlevel 1 exit /b %errorlevel%

.\target\release\rust-bot gateway --config ./configs/channels/config_anthropic_email.json %*
