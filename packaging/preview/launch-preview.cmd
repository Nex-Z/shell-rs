@echo off
setlocal
set "SHELLRS_PREVIEW_DATA_DIR=%APPDATA%\shellrs-preview"
set "SHELLRS_ANALYTICS_DEBUG="
start "" "%~dp0shellrs.exe"
endlocal
