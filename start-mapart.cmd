@echo off
rem Starts the mapart bot: it takes carpet map art sent to its Discord channel (#mapart) and builds
rem it on the water of its site, maps it, locks the map and stores it. Its Discord bot token is
rem bots\%BOT%\discord-token.txt.
rem Its settings are bots\%BOT%\config.json; its debug logs are in bots\%BOT%\debug\.
rem
rem The first start asks you to sign in (a microsoft.com/link address and a code in this window):
rem sign in with the account the mapart bot is to play as. That account is then saved as its own, and
rem every later start uses it without asking.
rem
rem If the bot stops it is started again 30 s later - except after a deliberate logout (exit code 3:
rem a player came into view, or Disconnect), which stays out until you run this again.
rem Close this window to stop it.

rem The bot's folder under bots\ (not its account name - that is picked at the first sign-in).
set BOT=Mapart

title Mapart (Rust) - %BOT%
cd /d "%~dp0"

if not exist "bots\%BOT%\config.json" (
  echo No bots\%BOT%\config.json yet - create it first ^("job":"mapart"^).
  pause
  exit /b 1
)

rem Never two copies of the same bot at once.
powershell -NoProfile -Command "if (Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'melonbot.exe' -and $_.CommandLine -match 'run %BOT%' }) { exit 1 } else { exit 0 }"
if errorlevel 1 (
  echo %BOT% is already running in another window. Close it first.
  pause
  exit /b 1
)

:loop
target\release\melonbot.exe run %BOT%
if %errorlevel%==3 (
  echo Logged out on purpose - not restarting. Run this again to go back on.
  pause
  exit /b 0
)
echo The bot stopped (exit %errorlevel%^) - starting again in 30 s. Close this window to stop.
timeout /t 30 /nobreak >nul
goto loop
