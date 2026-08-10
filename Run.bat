@echo off
REM Spider Charts launcher. Builds if needed, then starts the desktop app.
REM Pass --sync to run the headless nightly sync instead.

cd /d "%~dp0"

if "%~1"=="--sync" goto sync

echo Building Spider Charts (release)...
cargo build --release || goto fail
echo Starting...
"target\release\spider_charts.exe"
goto end

:sync
echo Running headless sync...
cargo build --release || goto fail
"target\release\spider_charts.exe" --sync
goto end

:fail
echo.
echo Build failed. Scroll up for the compiler output.
pause

:end
