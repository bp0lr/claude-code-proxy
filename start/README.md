# Launchers

Windows entry points for a built checkout. Every path is resolved relative to
this folder, so moving the whole checkout somewhere else keeps them working.

They all use the **release** binary. Build it once from the repository root:

```sh
cargo build --release
```

## `start-proxy.cmd`

Starts the proxy with its monitor on `http://127.0.0.1:18765` and keeps the
window open when it exits, so a startup failure stays readable. This is what
the desktop shortcut points at.

Traffic capture is on: the raw request and response of every call are written
under `%LOCALAPPDATA%\claude-code-proxy\traffic`, which is what makes a 502
diagnosable after the fact. The newest 200 captures are kept and older ones are
deleted automatically. Captures preserve prompts, tool inputs, tool results and
provider output in the clear — set `CCP_TRAFFIC_LOG=0` in the file to turn it
off.

## `prompt.ps1`

One-shot prompt through the running proxy, for use outside an agent. The model
ID picks the provider, so any model the proxy routes works. Reads from an
argument, a file, or the pipeline; writes the answer to stdout and optionally to
a file.

```powershell
.\prompt.ps1 -Model grok-4.6 "Write a two-page scene."
.\prompt.ps1 -Model gpt-5.6-sol -File .\outline.md -Out .\ch03.md
"Give me three alternative endings" | .\prompt.ps1 -Model kimi-k3
```

`-Model` is required unless `CCP_MCP_MODEL` is set. `-Verbose` adds the model
and token accounting. The proxy has to be running.

## `mcp-server.cmd`

Wrapper around `claude-code-proxy mcp`, the MCP server built into this binary.
MCP clients spawn it over stdio; it is not meant to be run interactively. It
exposes `generate`, which reaches any model the proxy routes, and `status`.

```json
{
  "mcpServers": {
    "grok": {
      "command": "G:\\Codigos\\Codigos\\lovan\\MisProgramas\\llm-proxy\\claude-code-proxy\\start\\mcp-server.cmd"
    }
  }
}
```

The proxy has to be running for `generate` to answer; `status` is what tells
you whether it is.
