@echo off
setlocal
cd /d "%~dp0.."

cargo build -r
cd web-chat
trunk build --release