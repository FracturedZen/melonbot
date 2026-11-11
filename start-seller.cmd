@echo off
rem Starts the Rust stick-seller bot. Set ACCOUNT below to the Minecraft account it signs in as
rem (that name is also the folder under bots\, e.g. bots\%ACCOUNT%\config.json).
rem The bot's log scrolls in this window; its debug logs are in bots\%ACCOUNT%\debug\.
rem
rem If the bot stops it is started again 30 s later - except after a deliberate logout (exit code 3:
rem a player came into view, or Disconnect), which stays out until you run this again.
rem Close this window to stop it.

rem ====== SET THIS to the bot's account name ======
set ACCOUNT=Shopper
rem ================================================

title Stick-seller (Rust) - %ACCOUNT%
cd /d "%~dp0"

if "%ACCOUNT%"=="CHANGE_ME" (
  echo Open start-seller.cmd and set ACCOUNT to the bot's Minecraft account name first.
  pause
  exit /b 1
)

if not exist "bots\%ACCOUNT%\config.json" (
  echo No bots\%ACCOUNT%\config.json yet - create it first ^(account + "job":"seller"^).
  pause
  exit /b 1
)

rem Never two copies of the same account at once.
powershell -NoProfile -Command "if (Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'melonbot.exe' -and $_.CommandLine -match 'run %ACCOUNT%' }) { exit 1 } else { exit 0 }"
if errorlevel 1 (
  echo %ACCOUNT% is already running in another window. Close it first.
  pause
  exit /b 1
)

:loop
target\release\melonbot.exe run %ACCOUNT%
if %errorlevel%==3 (
  echo Logged out on purpose - not restarting. Run this again to go back on.
  pause
  exit /b 0
)
echo The bot stopped (exit %errorlevel%^) - starting again in 30 s. Close this window to stop.
timeout /t 30 /nobreak >nul
goto loop
