# claude-code-proxy — setup reference

A self-contained guide to configuring this proxy. Written to be pasted into
another assistant's context as the authoritative description of how the tool
works.

Everything below was verified against the running build, not recalled from
documentation. Where a value changes per install, the command that prints the
real value is given instead of a literal.

---

## 1. What this is

`claude-code-proxy` exposes an **Anthropic-compatible HTTP API on localhost** and
translates each request into the native protocol of whichever provider the
requested model belongs to. Claude Code (or any Anthropic-API client) points at
it and keeps working unchanged, while the model that actually answers comes from
a subscription you already pay for.

**This is not a Grok tool.** Grok is one of five providers. The proxy is
provider-agnostic and routes per request:

| Provider | Account needed | Native protocol used upstream |
| --- | --- | --- |
| **Codex** | ChatGPT Plus or Pro (not an OpenAI API key) | OpenAI Responses over WebSocket or HTTP SSE |
| **Kimi** | kimi.com with Kimi Code access | OpenAI-style chat completions |
| **Grok** | grok.com | Responses API |
| **OpenCode Go** | OpenCode Go subscription (API key) | OpenAI-compatible / Responses / Anthropic Messages |
| **Cursor Agent** | Cursor account | HTTP/2 Connect stream |

One listener serves all of them at once. Routing is **per request, by model ID**
— not per process, not per port. Switching models mid-session can switch
providers.

### The two roles — do not confuse them

The binary does two independent things. Most setups only need the first.

**Role 1 — HTTP proxy (the main one).** The client sets
`ANTHROPIC_BASE_URL=http://127.0.0.1:18765` and talks Anthropic Messages. **This
has nothing to do with MCP.** Claude Code stops being Claude here: the responding
model is Grok/Codex/Kimi/Cursor/OpenCode.

**Role 2 — MCP server (optional).** `claude-code-proxy mcp` runs a stdio MCP
server that exposes the proxy's models as a *tool*. Here the agent stays
whatever it was and can additionally ask a different model a question. It routes
through the same proxy, so any provider is reachable.

The two are opposites, not alternatives. Role 1 **replaces** the model that
reasons; Role 2 **adds** a model the reasoning one can consult. If the goal is
"run this project on Codex/Grok/Kimi", that is Role 1 and MCP never enters the
picture.

### Security

The listener performs **no client authentication** — it accepts any request
without validating `Authorization` or `x-api-key`. It binds loopback by default.
Any non-loopback listener must be protected by a firewall or an authenticating
reverse proxy.

---

## 2. Quick start

```sh
# 1. Authenticate at least one provider (see §6)
claude-code-proxy codex auth login

# 2. Run the proxy and leave it running
claude-code-proxy serve            # opens the monitor TUI in a terminal
claude-code-proxy serve --no-monitor   # plain logs, for a service or a pane

# 3. Confirm it is up
curl http://127.0.0.1:18765/healthz     # -> {"ok":true}
claude-code-proxy models                # -> the live catalog

# 4. Point a client at it
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-5.6-sol[1m] \
ANTHROPIC_SMALL_FAST_MODEL=gpt-5.6-luna[1m] \
  claude
```

Default listener: `127.0.0.1:18765`. Override with `serve --port`, `PORT`, or
`"port"` in `config.json` — `--port` wins over both.

---

## 3. Client configuration

These variables belong to **Claude Code**, not to the proxy. Keep the two layers
separate: `ANTHROPIC_*` and `CLAUDE_CODE_*` configure the client, `CCP_*` and
`config.json` configure the proxy process (§7).

| Variable | Purpose |
| --- | --- |
| `ANTHROPIC_BASE_URL` | Points the client at the proxy. **Required.** |
| `ANTHROPIC_AUTH_TOKEN` | Satisfies the client's credential check. The proxy ignores it; upstream auth comes from the stored provider login. Any non-empty string works — `unused` by convention. |
| `ANTHROPIC_MODEL` | Main model, and therefore the provider. |
| `ANTHROPIC_SMALL_FAST_MODEL` | Model for background work: titles, token counting, small requests. Must also be routable. |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` | Cuts background traffic to the subscription provider. |
| `CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1` | Stops the client retrying a partial stream as non-streaming, which can duplicate tool calls. |
| `CLAUDE_CODE_AUTO_COMPACT_WINDOW` | Compaction threshold in tokens. Set it to match the real upstream window. |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | Declares the real context window for a model ID the client does not recognize. See §5.3. |

The client reads its API connection **at process start**. Changing any of these
requires a new `claude` process; it cannot be switched mid-session.

### 3.1 Per-project configuration (recommended)

Put the variables in `.claude/settings.json` **at the project root**. Only that
project is affected; every other project keeps talking to the real Anthropic API.

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "gpt-5.6-sol[1m]",
    "ANTHROPIC_SMALL_FAST_MODEL": "gpt-5.6-luna[1m]",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "272000",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1"
  }
}
```

No per-session setup, no asking an assistant to configure anything: any `claude`
started in that directory picks it up.

Scopes, strongest first:

| File | Scope | Committed |
| --- | --- | --- |
| `.claude/settings.local.json` | This project, you only | No — gitignored |
| `.claude/settings.json` | This project, everyone | Yes |
| `~/.claude/settings.json` | Every project | n/a — global |

A settings-file `env` value takes precedence over the same variable in the real
shell environment.

> **Use `settings.local.json` for proxy settings.** `ANTHROPIC_BASE_URL` pointing
> at `127.0.0.1` is only meaningful on a machine where the proxy is running.
> Committing it hands every collaborator a connection-refused they cannot
> diagnose.

### 3.2 One-shot, without touching any file

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=grok-4.6 ANTHROPIC_SMALL_FAST_MODEL=grok-4.6 \
  claude
```

Useful as a shell alias when you want to keep the default backend intact.

---

## 4. Recommended presets

Copy one block into `.claude/settings.local.json`. Each assumes that provider is
authenticated (§6) and the proxy is running.

### Codex — ChatGPT Plus/Pro

The best general-purpose default when you have a ChatGPT subscription.

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "gpt-5.6-sol[1m]",
    "ANTHROPIC_SMALL_FAST_MODEL": "gpt-5.6-luna[1m]",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "272000",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1"
  }
}
```

`272000` matches the GPT-5.6 subscription context limit. Swap `sol` for `terra`
if you want the larger sibling, or append `-fast` to either for the priority
service tier.

### Grok

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "grok-4.6",
    "ANTHROPIC_SMALL_FAST_MODEL": "grok-4.6",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1"
  }
}
```

No `[1m]` and no window override: the client does not recognize Grok IDs and
falls back to assuming 200k tokens, which is conservative but safe. Raise it
deliberately with `CLAUDE_CODE_MAX_CONTEXT_TOKENS` once you know the real window
for your account. Expect the warning in §5.3 either way.

`grok-composer-2.5-fast` is the cheap/fast option for the small model slot.

Grok effort levels are `none`, `low`, `medium`, `high`, `xhigh`, `max`. `xhigh`
runs at full strength only on `grok-4.6`; on other Grok models `xhigh` and `max`
map down to `high`.

### Kimi

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "kimi-k3[1m]",
    "ANTHROPIC_SMALL_FAST_MODEL": "kimi-k2.6",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1000000",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1"
  }
}
```

K3 genuinely has a one-million-token context window, so `[1m]` and the matching
compaction threshold are honest here rather than aspirational. K3 also accepts
`max` reasoning effort.

### OpenCode Go

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "opencode-go/qwen3.8-max",
    "ANTHROPIC_SMALL_FAST_MODEL": "opencode-go/deepseek-v4-flash",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1"
  }
}
```

Prefer the `opencode-go/` prefixed form. Several bare IDs (`kimi-k2.6`,
`grok-4.5`, `gpt-5.6-luna`) also exist in other providers' catalogs, and the bare
ID routes to the other provider. The prefix removes the ambiguity.

### Cursor Agent

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "cursor:composer-2.5",
    "ANTHROPIC_SMALL_FAST_MODEL": "cursor:composer-2.5-fast",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1"
  }
}
```

Cursor's catalog is discovered at runtime from the installed Cursor Agent
bundle, so the alias list is empty until you authenticate. Run
`claude-code-proxy models --full` to see the real set. Cursor's native tool
bridge only covers `Read`, `Write` and `Bash`, and requires streaming plus a
stable session header.

### Mixing providers

The main and small models do not have to share a provider — routing is per
request. A large model for the work and a cheap one for background traffic is a
legitimate combination:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765",
    "ANTHROPIC_AUTH_TOKEN": "unused",
    "ANTHROPIC_MODEL": "gpt-5.6-sol[1m]",
    "ANTHROPIC_SMALL_FAST_MODEL": "grok-composer-2.5-fast",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "272000",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1"
  }
}
```

Both providers must be authenticated for this to work.

---

## 5. Models and routing

### 5.1 Getting the real list

The catalog changes with provider access and build. **Never rely on a
hardcoded list** — ask the installation:

```sh
claude-code-proxy models          # compact, abbreviates Cursor aliases
claude-code-proxy models --full   # every Cursor alias
curl http://127.0.0.1:18765/v1/models
```

Catalog at the time of writing:

- **codex** — `gpt-5.2`, `gpt-5.3-codex`, `gpt-5.3-codex-spark`, `gpt-5.4`,
  `gpt-5.4-mini`, `gpt-5.5`, `gpt-5.6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`,
  each also in a `-fast` form; plus the Anthropic-style aliases `fable`,
  `haiku`, `opus`, `sonnet`, `claude-fable-5`, `claude-haiku-4-5`,
  `claude-opus-4-7`, `claude-opus-4-8`, `claude-opus-5`, `claude-sonnet-4-6`,
  `claude-sonnet-5`
- **kimi** — `kimi-for-coding`, `kimi-k2.6`, `kimi-k3`, `k2.6`, `k3`
- **grok** — `grok-4.5`, `grok-4.6`, `grok-composer-2.5-fast`
- **opencode** — `deepseek-v4-flash`, `deepseek-v4-pro`, `glm-5`, `glm-5.1`,
  `glm-5.2`, `hy3`, `kimi-k2.5`, `kimi-k2.7-code`, `mimo-v2.5`, `mimo-v2.5-pro`,
  `minimax-m2.5`, `minimax-m2.7`, `minimax-m3`, `qwen3.5-plus`, `qwen3.6-plus`,
  `qwen3.7-max`, `qwen3.7-plus`, `qwen3.8-max`, each also as
  `opencode-go/<id>`, plus `opencode-go/gpt-5.6-luna`, `opencode-go/grok-4.5`,
  `opencode-go/kimi-k2.6`, `opencode-go/kimi-k3`
- **cursor** — `cursor`, `cursor-agent`, `cursor-ask`, `cursor-composer`,
  `cursor-composer-fast`, `cursor-plan`, `composer-2.5`, `composer-2.5-fast`,
  plus runtime aliases

### 5.2 Routing rules

| Model ID pattern | Goes to |
| --- | --- |
| Registered `gpt-*` and their `-fast` forms | Codex |
| `kimi-for-coding`, `kimi-k2.6`, `kimi-k3`, `k2.6`, `k3` | Kimi |
| `grok-4.5`, `grok-4.6`, `grok-composer-2.5-fast` | Grok |
| Non-conflicting OpenCode IDs, and every `opencode-go/<id>` | OpenCode Go |
| `cursor`, `cursor:<id>`, `cursor-plan:<id>`, `cursor-ask:<id>` | Cursor Agent |
| `haiku`, `sonnet`, `opus`, `fable`, `claude-*` | The **alias provider** |

An unknown ID returns HTTP 400 with the supported catalog. There is no implicit
fallback.

**Alias provider.** Anthropic-style names do not mean Anthropic — they are
routed to whatever `aliasProvider` says, **Codex by default**. Accepted values
are `codex` and `kimi`; set with `CCP_ALIAS_PROVIDER` or `"aliasProvider"` in
`config.json`. Explicit provider IDs always use their own provider regardless.

So `ANTHROPIC_MODEL=opus` through this proxy does **not** reach Claude Opus. It
reaches whatever the alias provider maps it to. Use concrete IDs to stay
explicit.

**Codex `-fast`.** Every Codex model has a local `-fast` form. The proxy strips
the suffix upstream and requests the priority service tier. An explicit
`codex.serviceTier` / `CCP_CODEX_SERVICE_TIER` overrides it.

### 5.3 The `[1m]` suffix and the unknown-model warning

A trailing `[1m]` is a hint to **Claude Code's local compaction policy**, not a
capability. The proxy strips it before routing. It does not enlarge any
provider's context window — using it on a model that cannot take the context
just moves the failure later.

For model IDs the client does not recognize (every non-Anthropic ID), expect:

> `"grok-4.5" is not a model this version of Claude Code recognizes, so
> auto-compact will keep this session within 200k tokens (the context window it
> assumes). If the model accepts more, append [1m] to the model name for 1M, or
> set CLAUDE_CODE_MAX_CONTEXT_TOKENS to its real window`

It is a warning, not an error — the session works. Silence it by setting
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` to the model's real window, or by accepting the
conservative 200k default.

### 5.4 Reasoning effort

Effort travels per request — Claude Code's `/effort`, or whatever the harness
sets for a given agent — and **the request wins**.

`CCP_CODEX_EFFORT` / `codex.effort` is the *default* for requests that name no
effort, not a replacement for ones that do. That ordering is what allows one
session to drive several models at different efforts: one agent on
`gpt-5.6-sol` at medium alongside another on `gpt-5.6-luna` at high. A
configured effort that overrode the request would collapse both to one value.

Levels per provider:

| Provider | Accepted | Notes |
| --- | --- | --- |
| Codex | `low`, `medium`, `high`, `xhigh`, `max` | plus `none` from the configured default |
| Grok | `none`, `low`, `medium`, `high`, `xhigh`, `max` | `xhigh` at full strength only on `grok-4.6`; `xhigh`/`max` map to `high` elsewhere |
| Kimi | `low`, `medium`, `high` | K3 also takes `max` |
| OpenCode Go | varies by model | `glm-5.x` rejects `low`/`medium`; `mimo` rejects `xhigh`/`max`; other models discard it |
| Cursor | — | no effort field; the catalog carries effort variants in the model ID instead |

One exception, and it is deliberate: `CCP_COMPACT_EFFORT` (default `low`) caps
effort on Claude Code's summary-compaction requests, which run extraction over a
whole transcript. It is a ceiling, never a raise, and applies only to those
requests. `off` disables it.

### 5.5 Switching

A **model** switch stays in the same session — `/model`, `--model`, or a new
`ANTHROPIC_MODEL`. Because routing is per request, this can also switch
providers.

A **backend** switch (proxy ↔ direct Anthropic) needs a new process, because
`ANTHROPIC_BASE_URL` is read at startup.

---

## 6. Provider authentication

Credentials are stored by the proxy, per provider. The client's
`ANTHROPIC_AUTH_TOKEN` plays no part in upstream auth.

```sh
claude-code-proxy <provider> auth <login|device|status|logout>
```

| Provider | `login` | `device` | `status` | `logout` |
| --- | --- | --- | --- | --- |
| `codex` | Browser PKCE | Device code | Account, expiry, storage | Deletes local credential |
| `kimi` | Device code | — | User, expiry, scope | Deletes local credential |
| `grok` | Browser PKCE | Device code | Expiry and storage | Deletes local credential |
| `cursor` | Browser polling | — | Source, claims, expiry | Deletes local credential |

`auth status` exits 1 when the credential is missing. Logout removes only the
local copy; it does not revoke anything upstream.

**OpenCode Go uses an API key, not a login flow.** Supply it through
`CCP_OPENCODE_API_KEY` (preferred), `OPENCODE_API_KEY`, or `"opencode":
{"apiKey": ...}` in `config.json`.

---

## 7. Proxy-side configuration

Separate layer from the client. Precedence: **environment variable → config file
→ built-in default**, except `serve --port`, which beats everything.

`config.json` lives directly under the config root:

| OS | Config root | State root (logs, captures) |
| --- | --- | --- |
| Windows | `%APPDATA%\claude-code-proxy` | `%LOCALAPPDATA%\claude-code-proxy` |
| macOS | `~/.config/claude-code-proxy` | `${XDG_STATE_HOME:-~/.local/state}/claude-code-proxy` |
| Linux | `${XDG_CONFIG_HOME:-~/.config}/claude-code-proxy` | `${XDG_STATE_HOME:-~/.local/state}/claude-code-proxy` |

`CCP_CONFIG_DIR` replaces the configuration root for the current process; it does
not move the state root.

```json
{
  "bindAddress": "127.0.0.1",
  "port": 18765,
  "aliasProvider": "codex",
  "codex": { "model": "gpt-5.6-sol", "effort": "high", "transport": "websocket" },
  "grok":  { "model": "grok-4.6" },
  "opencode": { "apiKey": "..." },
  "log": { "stderr": false, "verbose": false }
}
```

`codex.effort` there is a **default**, not an override: a request that names its
own effort keeps it. See [§5.4](#54-reasoning-effort).

All keys optional. A malformed file is ignored wholesale in favour of
environment values and defaults — it does not fail loudly, so verify with
`claude-code-proxy models` or the startup banner if a setting seems ignored.

Every key has a `CCP_*` environment equivalent (`CCP_ALIAS_PROVIDER`,
`CCP_CODEX_EFFORT`, `CCP_GROK_MODEL`, …). The canonical table is in the
project's `docs/reference/configuration.md`.

Two worth knowing:

- `CCP_TRAFFIC_LOG=1` captures the raw request and response of every call under
  the state root. It is what makes a 502 diagnosable after the fact. Captures
  **preserve prompts, tool inputs, tool results and provider output in the
  clear** — treat the directory as sensitive. The newest 200 are kept.
- `CCP_CODEX_RESPONSES_API=1` enables the OpenAI-compatible routes (§9).

---

## 8. MCP server (optional)

Independent of everything above. Exposes the proxy's models as a **tool** to an
agent that otherwise stays as it is.

```sh
claude-code-proxy mcp          # stdio, JSON-RPC 2.0, spawned by the client
```

Tools: `generate` (text from any model the proxy routes) and `status` (is the
proxy up). The `model` argument picks the provider exactly as it does on the
HTTP routes, so one MCP entry covers all five. It answers **through the running
proxy**, so the proxy must be up for `generate`; `status` is what tells you
whether it is.

`model` is required unless a default is configured through `CCP_MCP_MODEL` or
`mcp.model`. There is deliberately no built-in default: with five providers,
picking one here would be an arbitrary preference. An unknown ID comes back with
the supported catalog, and the reply reports which model answered.

Per-project registration — `.mcp.json` at the project root, committed:

```json
{
  "mcpServers": {
    "ccp": {
      "type": "stdio",
      "command": "claude-code-proxy",
      "args": ["mcp"],
      "env": {"CCP_MCP_MODEL": "grok-4.6"}
    }
  }
}
```

Or by CLI: `claude mcp add --scope project ccp -e CCP_MCP_MODEL=grok-4.6 -- claude-code-proxy mcp`

**Check that the binary is actually on `PATH` first** (`command -v
claude-code-proxy`). A source checkout that was never installed globally is not,
and the bare name fails with a spawn error that reads like an MCP protocol
problem. Use an absolute path in that case:

```json
{
  "mcpServers": {
    "ccp": {
      "type": "stdio",
      "command": "C:/path/to/claude-code-proxy/target/release/claude-code-proxy.exe",
      "args": ["mcp"]
    }
  }
}
```

MCP scopes, strongest first:

| Scope | Stored in | Committed | Flag |
| --- | --- | --- | --- |
| `local` (default) | `~/.claude.json`, under the project | No | none |
| `project` | `.mcp.json` in the repo | Yes | `--scope project` |
| `user` | `~/.claude.json` root | No | `--scope user` |

A name present in several scopes resolves to the strongest one; entries are
taken whole and never field-merged.

On Windows, `command` may be a bare name resolved on `PATH` or an absolute path.
In JSON, escape backslashes (`C:\\path`) or use forward slashes.

---

## 9. Non-Anthropic clients

With `CCP_CODEX_RESPONSES_API=1` (or `"codex": {"responsesApi": true}`) the proxy
also serves:

- `POST /v1/chat/completions` — OpenAI Chat Completions
- `POST /v1/responses` — OpenAI Responses

Both select the provider from `model`, exactly like `/v1/messages`. Codex models
use native passthrough; Kimi, Grok, OpenCode Go and Cursor go through
translation and accept messages, text, images, function tools, `tool_choice`,
`parallel_tool_calls`, token limits, reasoning effort, and streaming.

```sh
curl http://127.0.0.1:18765/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"grok-4.6","messages":[{"role":"user","content":"Hello"}]}'
```

Incoming bearer credentials are ignored; the stored provider login is used.
Unsupported non-null fields return `invalid_request_error` naming the field in
`error.param` rather than being silently dropped.

Other routes: `GET /healthz`, `GET /v1/models`, and
`POST /v1/messages/count_tokens` (a local estimate, not a billing count).
Optional image and transcription routes exist behind their own flags.

---

## 10. Troubleshooting

| Symptom | Cause |
| --- | --- |
| `connection refused` | Proxy not running. `curl http://127.0.0.1:18765/healthz`. |
| HTTP 400 with a model list | Unrecognized model ID. Compare against `claude-code-proxy models`. |
| HTTP 401 | Provider credential missing or expired. `<provider> auth status`. |
| Answers come from the wrong model | An alias (`opus`, `sonnet`) routed through `aliasProvider`. Use a concrete ID. |
| "not a model this version recognizes" | Expected for non-Anthropic IDs. §5.3. |
| Settings change had no effect | The client reads its connection at startup. Restart `claude`. |
| A setting in `config.json` is ignored | The file may be malformed and silently discarded. Check the startup banner. |

Diagnostic order:

1. `curl http://127.0.0.1:18765/healthz`
2. `claude-code-proxy models` — is the ID routable
3. `claude-code-proxy <provider> auth status`
4. The monitor TUI, or `proxy.log` under the state root
5. The redacted payload under `errors/` for a failed response
6. `CCP_TRAFFIC_LOG=1` for a full capture of a reproduction

### Verification checklist

```sh
curl -s http://127.0.0.1:18765/healthz                  # {"ok":true}
claude-code-proxy models                                # target ID present
claude-code-proxy <provider> auth status                # authenticated
cd <project> && claude -p "Reply with exactly: PROXY_OK"
```

Then confirm the request actually traversed the proxy — the log line names the
resolved provider:

```json
{"msg":"request_completed","fields":{"model":"grok-4.5","provider":"grok","status":200}}
```

That last step is the one that distinguishes "the client answered" from "the
client answered *through the proxy*". They look identical from the terminal.
