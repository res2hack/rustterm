@echo off
title RustTerm Launcher
cd /d "%~dp0RustTerm"

REM Kill any dangling zombie processes if any
taskkill /f /im tauri-app.exe >nul 2>&1
taskkill /f /im RustTerm.exe >nul 2>&1

start "" "%~dp0RustTerm\RustTerm.exe"
exit
