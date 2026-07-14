@echo off
setlocal

cargo build -r
if errorlevel 1 exit /b %errorlevel%

set "SESSION_ARGS="

if /I "%~1"=="--session" (

    if not "%~2"=="" (

        set "SESSION_ARGS=--session %~2"

    )

)

@REM .\target\release\rust-bot agent --config ./configs/anthropic/config_gmail_anthropic.json --logs %SESSION_ARGS%
.\target\release\rust-bot agent --config ./configs/openai-compat/config_xai_groq.json --logs %SESSION_ARGS%


