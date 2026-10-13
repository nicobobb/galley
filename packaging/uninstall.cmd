@echo off
rem Double-click to uninstall Galley.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" -Uninstall
pause
