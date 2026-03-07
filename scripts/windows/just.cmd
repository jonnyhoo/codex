@echo off
setlocal

set "SCRIPT_DIR=%~dp0"
set "JUST_BIN=%USERPROFILE%\scoop\apps\just\current\just.exe"
if not exist "%JUST_BIN%" set "JUST_BIN=%USERPROFILE%\scoop\shims\just.exe"
if not exist "%JUST_BIN%" set "JUST_BIN=just.exe"

"%JUST_BIN%" --shell "%SCRIPT_DIR%bash-safe.cmd" --shell-arg -lc %*
exit /b %errorlevel%
