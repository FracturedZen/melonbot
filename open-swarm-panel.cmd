@echo off
rem Opens the BOTSWARM panel (the swarm bots - not a bot's own panel) in Chrome.
set URL=http://127.0.0.1:7000/
if exist "%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe" (
  start "" "%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe" %URL%
) else if exist "%ProgramFiles%\Google\Chrome\Application\chrome.exe" (
  start "" "%ProgramFiles%\Google\Chrome\Application\chrome.exe" %URL%
) else (
  start "" %URL%
)
