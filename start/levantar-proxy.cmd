@echo off
title Proxy Grok
echo Proxy Grok - 127.0.0.1:18765
echo Ctrl+C o cerrar la ventana para detenerlo.

rem Captura de trafico: guarda pedido y respuesta crudos de cada request en
rem %LOCALAPPDATA%\claude-code-proxy\traffic, para diagnosticar los 502.
rem Se conservan las ultimas 200 y se borran las viejas solas.
rem Para apagarla, poner 0 en la linea de abajo.
set CCP_TRAFFIC_LOG=1

if "%CCP_TRAFFIC_LOG%"=="1" echo Captura de trafico: ACTIVADA
echo.

rem %~dp0 es la carpeta de este archivo, asi que la ruta sigue valiendo
rem si se mueve la carpeta entera a otro lado.
"%~dp0claude-code-proxy\target\release\claude-code-proxy.exe" serve

echo.
echo El proxy se detuvo (codigo %ERRORLEVEL%).
echo Si fue un error de arranque, el motivo esta arriba.
pause
