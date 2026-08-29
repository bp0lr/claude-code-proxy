@echo off
setlocal
title claude-code-proxy

rem %~dp0 is this file's own folder, so the paths below survive moving the
rem whole checkout somewhere else. The launchers live in start\, one level
rem under the repository root.
set "ROOT=%~dp0.."
set "EXE=%ROOT%\target\release\claude-code-proxy.exe"

if not exist "%EXE%" (
    echo The release binary is missing:
    echo   %EXE%
    echo.
    echo Build it first, from the repository root:
    echo   cargo build --release
    echo.
    pause
    exit /b 1
)

rem Traffic capture writes the raw request and response of every call under
rem %%LOCALAPPDATA%%\claude-code-proxy\traffic, which is what makes a 502
rem diagnosable after the fact. The newest 200 captures are kept and older
rem ones are deleted automatically.
rem
rem Captures preserve prompts, tool inputs, tool results and provider output
rem in the clear. Set this to 0 to turn it off.
set CCP_TRAFFIC_LOG=1

echo claude-code-proxy - http://127.0.0.1:18765
if "%CCP_TRAFFIC_LOG%"=="1" echo Traffic capture: ON
echo Press Ctrl+C or close this window to stop it.
echo.

"%EXE%" serve
set "CODE=%ERRORLEVEL%"

echo.
echo The proxy stopped (exit code %CODE%).
if not "%CODE%"=="0" echo If this was a startup failure, the reason is above.
pause
exit /b %CODE%
