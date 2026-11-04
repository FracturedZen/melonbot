@echo off
rem Opens the PERIMETER panel (the perimeter bots - not the botswarm's or a bot's own) in Chrome.
set URL=http://127.0.0.1:7100/
if exist "%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe" (
  start "" "%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe" %URL%
) else if exist "%ProgramFiles%\Google\Chrome\Application\chrome.exe" (
  start "" "%ProgramFiles%\Google\Chrome\Application\chrome.exe" %URL%
) else (
  start "" %URL%
)
