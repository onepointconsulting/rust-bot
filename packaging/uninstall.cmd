@echo off
rem Runs uninstall.ps1 next to this file; -ExecutionPolicy Bypass because the default policy blocks .ps1 files.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0uninstall.ps1" %*
