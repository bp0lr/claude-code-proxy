---
title: Command reference
description: Canonical claude-code-proxy command syntax for serving, monitoring, listing models, version output, and provider authentication.
---

Running `claude-code-proxy` without a subcommand is equivalent to `claude-code-proxy serve`.

## Global version commands

```sh
claude-code-proxy --version
claude-code-proxy -v
claude-code-proxy version
```

Each prints `claude-code-proxy <version>`.

## `serve`

```sh
claude-code-proxy serve [--port <PORT>] [--no-monitor]
```

Starts the local HTTP proxy and blocks until shutdown.

| Option | Behavior |
| --- | --- |
| `--port <PORT>` | Overrides `PORT`, `config.json`, and the default for this invocation. |
| `--no-monitor` | Uses plain output even when stdout is a terminal. |

The bind address comes from `CCP_BIND_ADDRESS` or `bindAddress`. Interactive stdout opens the monitor unless `--no-monitor` is present. Non-terminal stdout uses plain mode.

## `demo`

```sh
claude-code-proxy demo
```

Opens the monitor with deterministic simulated traffic. It does not bind a network port or contact providers.

## `models`

```sh
claude-code-proxy models [--full]
```

Prints supported IDs grouped by provider. The default output compacts Cursor's runtime catalog. `--full` prints every Cursor alias.

## Provider authentication

The command shape is:

```text
claude-code-proxy <provider> auth <action>
```

| Provider | `login` | `device` | `status` | `logout` |
| --- | --- | --- | --- | --- |
| `codex` | Browser PKCE | Device code | Account, expiry, storage | Delete proxy credential |
| `kimi` | Device code | Unsupported | User, expiry, scope, storage | Delete proxy credential |
| `grok` | Browser PKCE | Device code | Expiry and storage | Delete proxy credential |
| `cursor` | Browser polling flow | Unsupported | Source, claims, expiry | Delete proxy credential |

Examples:

```sh
claude-code-proxy codex auth login
claude-code-proxy grok auth device
claude-code-proxy kimi auth status
claude-code-proxy cursor auth logout
```

A missing credential makes `auth status` exit with status 1. Other provider command failures exit with status 2. Successful commands exit with status 0.

Logout removes the local proxy-owned credential. It does not call the provider to revoke a refresh token.

## `grok usage`

```sh
claude-code-proxy grok usage [--json]
```

Prints how much of the Grok plan window the account has consumed and when the window renews. This is the account limit that stops generation, not the per-request token counts the monitor shows. `--json` prints the raw document instead of the summary. The command needs a Grok login and exits with status 2 when the lookup fails.

The same figure is available over HTTP at [`GET /usage`](/reference/http-api/#get-usage), on the monitor header, and through the MCP `usage` tool. The launch banner prints it once at startup unless `CCP_GROK_USAGE_ON_START` or `grok.usageOnStart` turns that off.

## Development commands

From a source checkout:

```sh
cargo run -- serve
cargo test --all
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
just check
just docs
```

`just docs` installs the locked documentation dependencies and starts the Astro development server on an available local port.
