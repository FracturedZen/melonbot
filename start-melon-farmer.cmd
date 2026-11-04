@echo off
rem Starts the melon farmer: the melon routine bot (farm, builds, the control panel on :6971).
rem Its folder under bots is the first argument (start-melon-farmer.cmd MyFarmer), else
rem MelonFarmer - bots<folder>config.json (write it with `melonbot import <Java instance>`).
rem The bot's log scrolls in this window; its debug logs are in bots<folder>debug.
rem Control panel: http://127.0.0.1:6971/  (open-panel.cmd opens it in Chrome)
rem
rem If the bot stops it is started again 30 s later - except after a deliberate logout (exit code 3:
rem a player came into view, or Disconnect on the control panel), which stays out until you run
rem this again. Close this window to stop it.
set BOT=%~1
if "%BOT%"=="" set BOT=MelonFarmer

title Melon farmer (Rust) - %BOT%
cd /d "%~dp0"

if not exist "bots%BOT%config.json" (
  echo No bots%BOT%config.json yet - run: melonbot import "<your Java instance folder>"
  pause
  exit /b 1
)

rem Never two copies of the same bot at once. The botswarm is a different melonbot.exe.
powershell -NoProfile -Command "if (Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'melonbot.exe' -and $_.CommandLine -match 'run %BOT%' }) { exit 1 } else { exit 0 }"
if errorlevel 1 (
  echo %BOT% is already running in another window. Close it first.
  pause
  exit /b 1
)

:loop
targeteleasemelonbot.exe run %BOT%
if %errorlevel%==3 (
  echo Logged out on purpose - not restarting. Run this again to go back on.
  pause
  exit /b 0
)
echo The bot stopped (exit %errorlevel%^) - starting again in 30 s. Close this window to stop.
timeout /t 30 /nobreak >nul
goto loop
