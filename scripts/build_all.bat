@echo off
setlocal
cd /d "%~dp0.."

cargo build -r
if errorlevel 1 exit /b 1

cd web-chat
trunk build --release
if errorlevel 1 exit /b 1
cd ..

cd websockets-chat
trunk build --release
if errorlevel 1 exit /b 1
cd ..