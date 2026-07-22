@echo off
setlocal
cd /d "%~dp0.."

set RUST_LOG_FILE=./logs/rust-bot-gateway.log

cargo build
if errorlevel 1 exit /b %errorlevel%

.\target\debug\rust-bot gateway --config ./configs/channels/config_anthropic_whatsapp_self_chat.json
