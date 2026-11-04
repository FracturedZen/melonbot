@echo off
rem PERIMETER BOTS - clear a box (set on the perimeter panel) from build height to bedrock.
rem Every account ticked in the perimeter panel, all in this one window.             
rem NOT the botswarm, NOT a bot of its own: starting or closing this never touches them. The accounts
rem are the botswarm's (added in either panel) - an account runs in one of the two at a time.
rem
rem Perimeter panel: http://127.0.0.1:7100/  (open-perimeter-panel.cmd opens it in Chrome)
rem Its files: perimeter\  (job.json, progress.json, logs\, debug\, bots\<name>\debug\)
rem If it stops it is started again 30 s later. Close this window to stop every perimeter bot.
title PERIMETER (Rust) - perimeter bots, not the botswarm
color 6F
cd /d "%~dp0"

rem One perimeter process at a time.
powershell -NoProfile -Command "if (Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'melonbot.exe' -and $_.CommandLine -match ' perimeter' }) { exit 1 } else { exit 0 }"
if errorlevel 1 (
  echo The perimeter bots are already running in another window.
  pause
  exit /b 1
)

:loop
echo.
echo  ===== PERIMETER BOTS =====  panel: http://127.0.0.1:7100/
echo.
target\release\melonbot.exe perimeter
echo The perimeter bots stopped (exit %errorlevel%^) - starting again in 30 s. Close this window to stop.
timeout /t 30 /nobreak >nul
goto loop
