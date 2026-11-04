@echo off
rem BOTSWARM - the swarm bots: every account added in the swarm panel, all in this one window.
rem NOT the bots with a window of their own (start-melon-farmer.cmd, start-seller.cmd...), which
rem have their own panels; starting or closing this never touches them.
rem
rem Swarm panel: http://127.0.0.1:7000/  (open-swarm-panel.cmd opens it in Chrome)
rem Its debug logs: swarm\debug\ and swarm\bots\<name>\debug\
rem If the swarm stops it is started again 30 s later. Close this window to stop every swarm bot.
title BOTSWARM (Rust) - swarm bots
color 5F
cd /d "%~dp0"

rem One botswarm at a time.
powershell -NoProfile -Command "if (Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'melonbot.exe' -and $_.CommandLine -match ' swarm' }) { exit 1 } else { exit 0 }"
if errorlevel 1 (
  echo The botswarm is already running in another window.
  pause
  exit /b 1
)

:loop
echo.
echo  ===== BOTSWARM =====  swarm panel: http://127.0.0.1:7000/
echo.
target\release\melonbot.exe swarm
echo The botswarm stopped (exit %errorlevel%^) - starting again in 30 s. Close this window to stop.
timeout /t 30 /nobreak >nul
goto loop
