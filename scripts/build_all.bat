@echo off
setlocal
cd /d "%~dp0.."

set "CARGO_FLAGS=-j 4"
set "TRUNK_FLAGS="
:parse_args
if "%~1"=="" goto args_done
if /I "%~1"=="--release" (
    set "CARGO_FLAGS=-r -j 4"
    set "TRUNK_FLAGS=--release"
)
shift
goto parse_args
:args_done

cargo build %CARGO_FLAGS%
if errorlevel 1 exit /b 1

cd web-chat
trunk build %TRUNK_FLAGS%
if errorlevel 1 exit /b 1
cd ..

cd websockets-chat
trunk build %TRUNK_FLAGS%
if errorlevel 1 exit /b 1
cd ..
