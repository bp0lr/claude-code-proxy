@echo off
setlocal

rem Runs the MCP server this binary ships natively, on stdio. It replaces the
rem standalone grok-mcp.ts from the previous program: same protocol and tools,
rem but it reaches every provider the proxy routes rather than Grok alone, and
rem it reuses this project's own translation path instead of a second
rem implementation that has to be kept in step.
rem
rem MCP clients spawn this file; they do not run it interactively. Point the
rem client at the absolute path of this file with no arguments.

set "ROOT=%~dp0.."
set "EXE=%ROOT%\target\release\claude-code-proxy.exe"

rem Nothing but protocol frames may reach stdout, so a missing binary has to be
rem reported on stderr and through the exit code.
if not exist "%EXE%" (
    echo mcp-server: release binary missing at %EXE% 1>&2
    echo mcp-server: build it with "cargo build --release" 1>&2
    exit /b 1
)

"%EXE%" mcp %*
