@echo off
set "SDIR=%~dp0"
if "%SDIR:~-1%"=="\" set "SDIR=%SDIR:~0,-1%"
powershell -NoProfile -ExecutionPolicy Bypass -Command "Set-Location '%SDIR%'; . '.\setup.ps1'"
pause
