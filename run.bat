@echo off
REM ============================================================================
REM  Downpour launcher
REM  Starts the Downpour desktop app (Tauri dev mode: Rust core + Vite UI).
REM  On first run it installs the frontend dependencies automatically.
REM
REM  Usage:
REM    run.bat            Launch the app in dev mode (hot reload)
REM    run.bat build      Produce a production build/installer for this OS
REM    run.bat help       Show this help
REM ============================================================================

setlocal enabledelayedexpansion

REM Always operate from the directory this script lives in.
cd /d "%~dp0"

REM --- Sub-command dispatch ---------------------------------------------------
if /i "%~1"=="help"  goto :help
if /i "%~1"=="/?"    goto :help
if /i "%~1"=="-h"    goto :help
if /i "%~1"=="build" goto :build

goto :dev

REM ----------------------------------------------------------------------------
:check_tools
REM Verify the required toolchain is on PATH before doing anything heavy.
where node >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Node.js was not found on your PATH.
  echo         Install it from https://nodejs.org/ ^(LTS^) and re-run this script.
  exit /b 1
)
where npm >nul 2>nul
if errorlevel 1 (
  echo [ERROR] npm was not found on your PATH.
  echo         It ships with Node.js - reinstall Node.js if this is missing.
  exit /b 1
)
where cargo >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Rust/cargo was not found on your PATH.
  echo         Install the Rust toolchain from https://rustup.rs/ and re-run.
  exit /b 1
)
exit /b 0

REM ----------------------------------------------------------------------------
:ensure_deps
REM Install frontend dependencies only when node_modules is missing.
if not exist "node_modules" (
  echo [setup] Installing frontend dependencies ^(first run only^)...
  call npm install
  if errorlevel 1 (
    echo [ERROR] npm install failed. See the output above.
    exit /b 1
  )
)
exit /b 0

REM ----------------------------------------------------------------------------
:dev
call :check_tools || exit /b 1
call :ensure_deps || exit /b 1
echo [run] Starting Downpour ^(dev mode^). The window opens after the Rust build.
echo [run] First launch compiles the Rust core and can take a few minutes.
echo [run] Leave this window open while you use the app; press Ctrl+C to quit.
call npm run tauri dev
exit /b %errorlevel%

REM ----------------------------------------------------------------------------
:build
call :check_tools || exit /b 1
call :ensure_deps || exit /b 1
echo [build] Building a production Downpour binary/installer for this OS...
echo [build] Output lands in src-tauri\target\release\ ^(and \bundle\ for installers^).
call npm run tauri build
exit /b %errorlevel%

REM ----------------------------------------------------------------------------
:help
echo.
echo Downpour launcher
echo -----------------
echo   run.bat            Launch the app in dev mode ^(hot reload^)
echo   run.bat build      Produce a production build/installer for this OS
echo   run.bat help       Show this help
echo.
echo Requirements: Node.js ^(LTS^), npm, and the Rust toolchain ^(rustup^).
echo The first launch installs npm packages and compiles the Rust core.
echo.
exit /b 0
