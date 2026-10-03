@echo off
REM Spider Charts launcher.
REM
REM   Run.bat            the desktop app (Tauri + React) - builds if needed
REM   Run.bat --old      the original egui app
REM   Run.bat --tail     headless daily update from bhavcopy, no login needed
REM   Run.bat --sync     headless full sync (no login needed; Upstox keys are optional)

cd /d "%~dp0"

if "%~1"=="--old"  goto old
if "%~1"=="--tail" goto tail
if "%~1"=="--sync" goto sync

REM Prefer an already-built release exe so a normal launch is instant.
if exist "target\release\spider-ui.exe" (
  start "" "target\release\spider-ui.exe"
  goto end
)

echo Building the desktop app. The first build takes a few minutes...
if not exist "spider-ui\node_modules" (
  echo Installing the web part first...
  call npm install --prefix spider-ui || goto fail
)
call npm run tauri build --prefix spider-ui || goto fail
start "" "target\release\spider-ui.exe"
goto end

:old
echo Building the egui app (release)...
cargo build --release || goto fail
start "" "target\release\spider_charts.exe"
goto end

:tail
cargo build --release || goto fail
"target\release\spider_charts.exe" --tail
goto end

:sync
cargo build --release || goto fail
"target\release\spider_charts.exe" --sync
goto end

:fail
echo.
echo Build failed. Scroll up for the compiler output.
pause

:end
