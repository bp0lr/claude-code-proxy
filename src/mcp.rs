//! MCP server over stdio, exposing the proxy's models as a tool.
//!
//! Speaks JSON-RPC 2.0 on stdin/stdout and forwards generation requests to the
//! proxy's own Anthropic-compatible endpoint, so every call shows up in the
//! monitor and reuses the existing translation path.
//!
//! Model IDs are not validated here. The proxy routes every registered model
//! to its provider and answers an unknown one with the supported catalog, so
//! duplicating that list would only let the two drift apart — and Cursor's
//! catalog is discovered at runtime, which a local copy could not know.
//!
//! Nothing but protocol frames may ever reach stdout.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_MAX_TOKENS: u64 = 16384;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// One completed generation: the text plus what it cost.
///
/// The caller needs the accounting to manage spend, and it must not be mixed
/// into the text, so it travels separately and is emitted as its own content
/// block.
#[derive(Debug)]
pub struct Generation {
    pub text: String,
    /// Which model actually answered. Worth reporting now that any of the
    /// proxy's providers can be the one that did.
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub stop_reason: Option<String>,
}

impl Generation {
    /// One-line accounting summary, or `None` when the proxy reported nothing.
    fn usage_line(&self) -> Option<String> {
        let stop = self.stop_reason.as_deref().unwrap_or("unknown");
        match (self.input_tokens, self.output_tokens) {
            (None, None) => None,
            (input, output) => Some(format!(
                "[usage] model={} input={} output={} stop={}{}",
                self.model.as_deref().unwrap_or("?"),
                input.map_or_else(|| "?".into(), |value| value.to_string()),
                output.map_or_else(|| "?".into(), |value| value.to_string()),
                stop,
                if stop == "max_tokens" {
                    " — the output was truncated by max_tokens"
                } else {
                    ""
                }
            )),
        }
    }
}

/// Where `generate` sends its work. Abstracted so the protocol layer can be
/// tested without a live proxy.
pub trait Backend {
    fn generate(&self, args: &Value) -> Result<Generation, String>;
    fn status(&self) -> String;
}

pub struct HttpBackend {
    client: reqwest::blocking::Client,
    port: u16,
}

impl HttpBackend {
    pub fn new(port: u16) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            port,
        })
    }

    fn messages_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1/messages", self.port)
    }

    fn unreachable(&self, reason: &str) -> String {
        format!(
            "Could not reach the proxy on 127.0.0.1:{} ({reason}). \
             Start it with: claude-code-proxy serve --no-monitor",
            self.port
        )
    }
}

/// Builds the Anthropic request body. The proxy rejects unknown top-level
/// fields with a 400, so only whitelisted keys are ever sent.
///
/// `default_model` is passed in rather than read from config here, so the
/// caller owns that lookup and tests stay deterministic. It is optional
/// because there is no sensible provider-neutral fallback: with no configured
/// default the caller has to name a model.
fn build_request_body(args: &Value, default_model: Option<&str>) -> Result<Value, String> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or("prompt is required and cannot be empty")?;

    // An unknown ID is the proxy's call, not this layer's: it answers 400 with
    // the supported catalog, which is surfaced verbatim.
    let model = args
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .or(default_model)
        .ok_or(
            "model is required: no default is configured. Pass \"model\", or set \
             CCP_MCP_MODEL (or mcp.model in config.json). Run \
             'claude-code-proxy models' for the catalog.",
        )?;

    let max_tokens = match args.get("max_tokens") {
        None | Some(Value::Null) => DEFAULT_MAX_TOKENS,
        Some(value) => value
            .as_u64()
            .filter(|tokens| (1..=200_000).contains(tokens))
            .ok_or("max_tokens must be an integer between 1 and 200000")?,
    };

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": false,
        "messages": [{"role": "user", "content": prompt}],
    });

    if let Some(system) = args
        .get("system")
        .and_then(Value::as_str)
        .filter(|system| !system.is_empty())
    {
        body["system"] = json!(system);
    }

    if let Some(temperature) = args.get("temperature")
        && !temperature.is_null()
    {
        let value = temperature
            .as_f64()
            .filter(|value| (0.0..=2.0).contains(value))
            .ok_or("temperature must be between 0 and 2")?;
        body["temperature"] = json!(value);
    }

    Ok(body)
}

/// Pulls the `text` blocks and the accounting out of an Anthropic response.
///
/// `thinking` blocks are deliberately dropped: they are the model's reasoning,
/// not its answer. They are still billed, which is why the token counts travel
/// alongside the text.
fn extract_generation(payload: &Value) -> Result<Generation, String> {
    let text: String = payload
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let stop_reason = payload
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(str::to_string);

    if text.is_empty() {
        let stop = stop_reason.as_deref().unwrap_or("unknown");
        return Err(format!("The model returned no text (stop_reason: {stop})"));
    }

    Ok(Generation {
        text,
        model: payload
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_tokens: payload
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64),
        output_tokens: payload
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64),
        stop_reason,
    })
}

fn error_message(payload: &Value, status: u16, raw: &str) -> String {
    let detail = payload
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| raw.get(..raw.len().min(500)).unwrap_or(raw));
    format!("The proxy returned HTTP {status}: {detail}")
}

impl Backend for HttpBackend {
    fn generate(&self, args: &Value) -> Result<Generation, String> {
        let default = crate::config::mcp_model();
        let body = build_request_body(args, default.as_deref())?;
        let response = self
            .client
            .post(self.messages_url())
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", "unused")
            .json(&body)
            .send()
            .map_err(|error| self.unreachable(&error.to_string()))?;

        let status = response.status();
        let raw = response
            .text()
            .map_err(|error| format!("Could not read the proxy response: {error}"))?;
        let payload: Value = serde_json::from_str(&raw).map_err(|_| {
            let preview = raw.get(..raw.len().min(500)).unwrap_or(&raw);
            format!("Non-JSON response from the proxy (HTTP {status}): {preview}")
        })?;

        if !status.is_success() {
            return Err(error_message(&payload, status.as_u16(), &raw));
        }
        extract_generation(&payload)
    }

    fn status(&self) -> String {
        let url = format!("http://127.0.0.1:{}/healthz", self.port);
        match self.client.get(url).timeout(Duration::from_secs(5)).send() {
            Ok(response) if response.status().is_success() => {
                format!("Proxy is up on 127.0.0.1:{}.", self.port)
            }
            Ok(response) => format!(
                "The proxy answered HTTP {} on /healthz.",
                response.status().as_u16()
            ),
            Err(error) => self.unreachable(&error.to_string()),
        }
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "generate",
            "description": "Generate text through the local proxy, using any model it \
                routes: Codex, Kimi, Grok, OpenCode Go or Cursor. Meant for asking a \
                different model than the one running this session - a second opinion, \
                or long-form writing in another voice. Returns the raw text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The prompt. Sent as a user message."
                    },
                    "system": {
                        "type": "string",
                        "description": "System prompt: voice, format, constraints. Optional, \
                            but strongly recommended to keep separate calls consistent."
                    },
                    "model": {
                        "type": "string",
                        "description": "Any model ID the proxy routes, which also picks \
                            the provider. Required unless a default is configured. An \
                            unknown ID comes back with the supported catalog; \
                            'claude-code-proxy models' lists it too."
                    },
                    "max_tokens": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 200000,
                        "description": "Output token ceiling. Defaults to 16384."
                    },
                    "temperature": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 2,
                        "description": "0 is deterministic, 1 is varied. Omit to use the model default."
                    }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }
        },
        {
            "name": "status",
            "description": "Check that the proxy is up and answering. Use it when generate \
                fails, to tell a stopped proxy apart from an upstream error.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }
    ])
}

fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i32, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// The generated text first, the accounting as a separate block so it never
/// contaminates the output the caller is after.
fn tool_generation(generation: &Generation) -> Value {
    let mut content = vec![json!({"type": "text", "text": generation.text})];
    if let Some(usage) = generation.usage_line() {
        content.push(json!({"type": "text", "text": usage}));
    }
    json!({"content": content, "isError": false})
}

/// Handles one request. Returns `None` for notifications, which carry no id and
/// must not be answered.
pub fn dispatch(request: &Value, backend: &dyn Backend) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str)?;
    let id = request.get("id").cloned();

    match method {
        "initialize" => Some(result(
            id?,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "claude-code-proxy", "version": env!("CARGO_PKG_VERSION")},
            }),
        )),
        "ping" => Some(result(id?, json!({}))),
        "tools/list" => Some(result(id?, json!({"tools": tool_definitions()}))),
        "tools/call" => {
            let id = id?;
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let empty = json!({});
            let args = request.pointer("/params/arguments").unwrap_or(&empty);

            match name {
                "status" => Some(result(id, tool_text(backend.status(), false))),
                // Tool failures are reported as results with isError, not as
                // JSON-RPC errors, so the model can see and react to them.
                "generate" => Some(result(
                    id,
                    match backend.generate(args) {
                        Ok(generation) => tool_generation(&generation),
                        Err(message) => tool_text(message, true),
                    },
                )),
                other => Some(rpc_error(id, -32602, format!("Unknown tool: {other}"))),
            }
        }
        // Notifications have no id and get no reply.
        _ if method.starts_with("notifications/") => None,
        other => id.map(|id| rpc_error(id, -32601, format!("Method not implemented: {other}"))),
    }
}

pub fn serve_stdio(port: u16) -> anyhow::Result<()> {
    let backend = HttpBackend::new(port)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("mcp: invalid line: {error}");
                continue;
            }
        };
        if let Some(response) = dispatch(&request, &backend) {
            writeln!(stdout, "{response}")?;
            // Without an explicit flush the client waits forever.
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockBackend;

    impl Backend for MockBackend {
        fn generate(&self, args: &Value) -> Result<Generation, String> {
            build_request_body(args, Some("gpt-5.6-sol")).map(|body| Generation {
                text: body.to_string(),
                model: Some("gpt-5.6-sol".into()),
                input_tokens: Some(248),
                output_tokens: Some(825),
                stop_reason: Some("end_turn".into()),
            })
        }
        fn status(&self) -> String {
            "mock".into()
        }
    }

    fn call(name: &str, args: Value) -> Value {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args},
        });
        dispatch(&request, &MockBackend).expect("tools/call always replies")
    }

    #[test]
    fn initialize_reports_protocol_and_tool_capability() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let response = dispatch(&request, &MockBackend).unwrap();
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_get_no_response() {
        let request = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(dispatch(&request, &MockBackend).is_none());
    }

    #[test]
    fn tools_list_exposes_generate_and_status() {
        let request = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let response = dispatch(&request, &MockBackend).unwrap();
        let names: Vec<_> = response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["generate", "status"]);
    }

    #[test]
    fn unknown_tool_is_a_protocol_error() {
        let response = call("does-not-exist", json!({}));
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn unknown_method_is_a_protocol_error() {
        let request = json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"});
        let response = dispatch(&request, &MockBackend).unwrap();
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn empty_prompt_is_a_tool_error_not_a_protocol_error() {
        let response = call("generate", json!({"prompt": "   "}));
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("prompt is required")
        );
    }

    #[test]
    fn request_body_only_carries_fields_the_proxy_accepts() {
        let body = build_request_body(
            &json!({
                "prompt": "hello",
                "system": "you are a screenwriter",
                "temperature": 0.7,
                "max_tokens": 900,
            }),
            Some("grok-4.5"),
        )
        .unwrap();

        assert_eq!(body["model"], "grok-4.5");
        assert_eq!(body["max_tokens"], 900);
        assert_eq!(body["stream"], false);
        assert_eq!(body["system"], "you are a screenwriter");
        assert_eq!(body["messages"][0]["content"], "hello");

        let keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        for key in &keys {
            assert!(
                [
                    "model",
                    "max_tokens",
                    "stream",
                    "messages",
                    "system",
                    "temperature"
                ]
                .contains(&key.as_str()),
                "unexpected field: {key}"
            );
        }
    }

    #[test]
    fn configured_default_applies_but_an_explicit_model_wins() {
        let body = build_request_body(&json!({"prompt": "hello"}), Some("grok-composer-2.5-fast"))
            .unwrap();
        assert_eq!(body["model"], "grok-composer-2.5-fast");

        // The explicit model may belong to a different provider than the
        // default: this layer forwards the ID and the proxy does the routing.
        let body = build_request_body(
            &json!({"prompt": "hello", "model": "kimi-k3"}),
            Some("grok-4.5"),
        )
        .unwrap();
        assert_eq!(body["model"], "kimi-k3");
    }

    #[test]
    fn a_model_is_required_when_nothing_is_configured() {
        let error = build_request_body(&json!({"prompt": "hello"}), None)
            .expect_err("must ask for a model");
        assert!(error.contains("model is required"), "{error}");
        assert!(error.contains("CCP_MCP_MODEL"), "{error}");

        // An empty string is not a model either.
        assert!(build_request_body(&json!({"prompt": "hello", "model": ""}), None).is_err());
    }

    /// Every provider's IDs reach the proxy untouched. Validating them here
    /// would duplicate the registry and drift from it.
    #[test]
    fn forwards_model_ids_from_every_provider() {
        for model in [
            "gpt-5.6-sol",
            "kimi-k3",
            "grok-4.6",
            "opencode-go/qwen3.8-max",
            "cursor:composer-2.5",
            "sonnet",
        ] {
            let body = build_request_body(&json!({"prompt": "hello", "model": model}), None)
                .unwrap_or_else(|error| panic!("{model} rejected: {error}"));
            assert_eq!(body["model"], model);
        }
    }

    #[test]
    fn request_body_omits_absent_optionals() {
        let body = build_request_body(&json!({"prompt": "hello"}), Some("grok-4.5")).unwrap();
        assert!(body.get("system").is_none());
        assert!(body.get("temperature").is_none());
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn rejects_out_of_range_numbers() {
        let model = Some("grok-4.5");
        assert!(build_request_body(&json!({"prompt": "a", "temperature": 9}), model).is_err());
        assert!(build_request_body(&json!({"prompt": "a", "max_tokens": 0}), model).is_err());
        assert!(build_request_body(&json!({"prompt": "a", "max_tokens": "many"}), model).is_err());
    }

    #[test]
    fn usage_travels_in_its_own_block_after_the_text() {
        let response = call("generate", json!({"prompt": "hello"}));
        let content = response["result"]["content"].as_array().unwrap();

        assert_eq!(content.len(), 2);
        // The text block must stay free of accounting noise.
        assert!(!content[0]["text"].as_str().unwrap().contains("[usage]"));
        let usage = content[1]["text"].as_str().unwrap();
        assert!(usage.contains("model=gpt-5.6-sol"), "{usage}");
        assert!(usage.contains("input=248"), "{usage}");
        assert!(usage.contains("output=825"), "{usage}");
        assert_eq!(response["result"]["isError"], false);
    }

    #[test]
    fn a_truncated_generation_says_so_in_the_usage_line() {
        let generation = Generation {
            text: "half a scene".into(),
            model: Some("grok-4.6".into()),
            input_tokens: Some(10),
            output_tokens: Some(4096),
            stop_reason: Some("max_tokens".into()),
        };
        let usage = generation.usage_line().unwrap();
        assert!(usage.contains("truncated"), "{usage}");
    }

    #[test]
    fn usage_block_is_omitted_when_the_proxy_reports_nothing() {
        let generation = Generation {
            text: "text".into(),
            model: None,
            input_tokens: None,
            output_tokens: None,
            stop_reason: None,
        };
        assert!(generation.usage_line().is_none());
        let value = tool_generation(&generation);
        assert_eq!(value["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn extracts_usage_and_stop_reason_alongside_the_text() {
        let payload = json!({
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 34}
        });
        let generation = extract_generation(&payload).unwrap();
        assert_eq!(generation.text, "hello");
        assert_eq!(generation.input_tokens, Some(12));
        assert_eq!(generation.output_tokens, Some(34));
        assert_eq!(generation.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn extracts_and_joins_text_blocks_only() {
        let payload = json!({"content": [
            {"type": "thinking", "text": "ignore me"},
            {"type": "text", "text": "Scene 1. "},
            {"type": "text", "text": "Marta walks in."}
        ]});
        assert_eq!(
            extract_generation(&payload).unwrap().text,
            "Scene 1. Marta walks in."
        );
    }

    #[test]
    fn empty_content_reports_the_stop_reason() {
        let payload = json!({"content": [], "stop_reason": "max_tokens"});
        let error = extract_generation(&payload).expect_err("must fail");
        assert!(error.contains("max_tokens"));
    }

    #[test]
    fn surfaces_the_proxy_error_message() {
        let payload = json!({"error": {"message": "Unknown model \"grok-9\""}});
        let message = error_message(&payload, 400, "raw");
        assert!(message.contains("HTTP 400"));
        assert!(message.contains("grok-9"));
    }
}
