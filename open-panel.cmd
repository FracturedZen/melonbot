@echo off
rem Opens the melon farmer's control panel (the same page the Java clients serve) in Chrome.
set URL=http://127.0.0.1:6971/
if exist "%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe" (
  start "" "%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe" %URL%
) else if exist "%ProgramFiles%\Google\Chrome\Application\chrome.exe" (
  start "" "%ProgramFiles%\Google\Chrome\Application\chrome.exe" %URL%
) else (
  start "" %URL%
)
