//! MCP server over stdio, exposing Grok as a tool for MCP clients.
//!
//! Speaks JSON-RPC 2.0 on stdin/stdout and forwards generation requests to the
//! proxy's own Anthropic-compatible endpoint, so every call shows up in the
//! monitor and reuses the existing translation path.
//!
//! Nothing but protocol frames may ever reach stdout.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

use crate::providers::grok::translate::model_allowlist::assert_allowed_model;

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
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub stop_reason: Option<String>,
}

impl Generation {
    /// One-line accounting summary, or `None` when the proxy reported nothing.
    fn usage_line(&self) -> Option<String> {
        let stop = self.stop_reason.as_deref().unwrap_or("desconocido");
        match (self.input_tokens, self.output_tokens) {
            (None, None) => None,
            (input, output) => Some(format!(
                "[uso] entrada={} salida={} stop={}{}",
                input.map_or_else(|| "?".into(), |value| value.to_string()),
                output.map_or_else(|| "?".into(), |value| value.to_string()),
                stop,
                if stop == "max_tokens" {
                    " — la salida quedo truncada por max_tokens"
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
    /// How much of the Grok plan window the account has burned through.
    fn usage(&self) -> Result<String, String>;
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

    fn usage_url(&self) -> String {
        format!("http://127.0.0.1:{}/usage?format=text", self.port)
    }

    fn unreachable(&self, reason: &str) -> String {
        format!(
            "No se pudo contactar al proxy en 127.0.0.1:{} ({reason}). \
             Levantalo con: claude-code-proxy serve --no-monitor",
            self.port
        )
    }
}

/// Builds the Anthropic request body. The proxy rejects unknown top-level
/// fields with a 400, so only whitelisted keys are ever sent.
///
/// `default_model` is passed in rather than read from config here, so the
/// caller owns that lookup and tests stay deterministic.
fn build_request_body(args: &Value, default_model: &str) -> Result<Value, String> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or("prompt es obligatorio y no puede estar vacio")?;

    let model = args
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(default_model);
    assert_allowed_model(model).map_err(|_| {
        format!("Modelo no soportado: {model}. Opciones: grok-4.6, grok-4.5, grok-composer-2.5-fast")
    })?;

    let max_tokens = match args.get("max_tokens") {
        None | Some(Value::Null) => DEFAULT_MAX_TOKENS,
        Some(value) => value
            .as_u64()
            .filter(|tokens| (1..=200_000).contains(tokens))
            .ok_or("max_tokens debe ser un entero entre 1 y 200000")?,
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
            .ok_or("temperature debe estar entre 0 y 2")?;
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
        let stop = stop_reason.as_deref().unwrap_or("desconocido");
        return Err(format!("Grok no devolvio texto (stop_reason: {stop})"));
    }

    Ok(Generation {
        text,
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
    format!("El proxy devolvio HTTP {status}: {detail}")
}

impl Backend for HttpBackend {
    fn generate(&self, args: &Value) -> Result<Generation, String> {
        let body = build_request_body(args, &crate::config::grok_model())?;
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
            .map_err(|error| format!("No se pudo leer la respuesta del proxy: {error}"))?;
        let payload: Value = serde_json::from_str(&raw).map_err(|_| {
            let preview = raw.get(..raw.len().min(500)).unwrap_or(&raw);
            format!("Respuesta no-JSON del proxy (HTTP {status}): {preview}")
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
                format!("Proxy activo en 127.0.0.1:{}.", self.port)
            }
            Ok(response) => format!(
                "El proxy respondio HTTP {} en /healthz.",
                response.status().as_u16()
            ),
            Err(error) => self.unreachable(&error.to_string()),
        }
    }

    fn usage(&self) -> Result<String, String> {
        let response = self
            .client
            .get(self.usage_url())
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(|error| self.unreachable(&error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .map_err(|error| format!("No se pudo leer la respuesta del proxy: {error}"))?;
        if !status.is_success() {
            let payload: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            return Err(error_message(&payload, status.as_u16(), &body));
        }
        Ok(body.trim_end().to_string())
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "generate",
            "description": "Genera texto con Grok a traves del proxy local. Pensado para \
                redaccion de largo aliento (escenas, dialogos, escaletas) donde queres la \
                voz de Grok en lugar de la propia. Devuelve el texto crudo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "La consigna. Se manda como mensaje de usuario."
                    },
                    "system": {
                        "type": "string",
                        "description": "System prompt: voz, formato, restricciones. Opcional \
                            pero muy recomendado para mantener consistencia entre llamadas."
                    },
                    "model": {
                        "type": "string",
                        "enum": ["grok-4.6", "grok-4.5", "grok-composer-2.5-fast"],
                        "description": "Modelo. Default: grok-4.5. grok-4.6 es el mas nuevo."
                    },
                    "max_tokens": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 200000,
                        "description": "Tope de tokens de salida. Default: 16384."
                    },
                    "temperature": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 2,
                        "description": "0 deterministico, 1 variado. Omitir usa el default del modelo."
                    }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }
        },
        {
            "name": "status",
            "description": "Verifica que el proxy este levantado y responda. Usar cuando \
                generate falla, para distinguir proxy caido de error de autenticacion.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "usage",
            "description": "Cuanto lleva consumido la cuenta de Grok en su ventana de plan \
                (semanal), y cuando renueva. Es el limite que corta la generacion, no el \
                conteo de tokens de una llamada. Usar antes de un trabajo largo, o cuando \
                generate empieza a fallar por limite.",
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
                "serverInfo": {"name": "grok", "version": env!("CARGO_PKG_VERSION")},
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
                "usage" => Some(result(
                    id,
                    match backend.usage() {
                        Ok(summary) => tool_text(summary, false),
                        Err(message) => tool_text(message, true),
                    },
                )),
                // Tool failures are reported as results with isError, not as
                // JSON-RPC errors, so the model can see and react to them.
                "generate" => Some(result(
                    id,
                    match backend.generate(args) {
                        Ok(generation) => tool_generation(&generation),
                        Err(message) => tool_text(message, true),
                    },
                )),
                other => Some(rpc_error(
                    id,
                    -32602,
                    format!("Herramienta desconocida: {other}"),
                )),
            }
        }
        // Notifications have no id and get no reply.
        _ if method.starts_with("notifications/") => None,
        other => id.map(|id| rpc_error(id, -32601, format!("Metodo no implementado: {other}"))),
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
                eprintln!("mcp: linea invalida: {error}");
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
            build_request_body(args, "grok-4.5").map(|body| Generation {
                text: body.to_string(),
                input_tokens: Some(248),
                output_tokens: Some(825),
                stop_reason: Some("end_turn".into()),
            })
        }
        fn status(&self) -> String {
            "mock".into()
        }
        fn usage(&self) -> Result<String, String> {
            Ok("Grok · ventana semanal: 18% consumido".into())
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
    fn tools_list_exposes_generate_status_and_usage() {
        let request = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let response = dispatch(&request, &MockBackend).unwrap();
        let names: Vec<_> = response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["generate", "status", "usage"]);
    }

    #[test]
    fn usage_reports_the_plan_window() {
        let response = call("usage", json!({}));
        assert_eq!(response["result"]["isError"], false);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("ventana semanal")
        );
    }

    #[test]
    fn unknown_tool_is_a_protocol_error() {
        let response = call("inexistente", json!({}));
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
                .contains("prompt es obligatorio")
        );
    }

    #[test]
    fn request_body_only_carries_fields_the_proxy_accepts() {
        let body = build_request_body(
            &json!({
                "prompt": "hola",
                "system": "sos guionista",
                "temperature": 0.7,
                "max_tokens": 900,
            }),
            "grok-4.5",
        )
        .unwrap();

        assert_eq!(body["model"], "grok-4.5");
        assert_eq!(body["max_tokens"], 900);
        assert_eq!(body["stream"], false);
        assert_eq!(body["system"], "sos guionista");
        assert_eq!(body["messages"][0]["content"], "hola");

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
                "campo inesperado: {key}"
            );
        }
    }

    #[test]
    fn configured_default_applies_but_an_explicit_model_wins() {
        let body =
            build_request_body(&json!({"prompt": "hola"}), "grok-composer-2.5-fast").unwrap();
        assert_eq!(body["model"], "grok-composer-2.5-fast");

        let body = build_request_body(&json!({"prompt": "hola", "model": "grok-4.5"}), "grok-4.5")
            .unwrap();
        assert_eq!(body["model"], "grok-4.5");
    }

    #[test]
    fn request_body_omits_absent_optionals() {
        let body = build_request_body(&json!({"prompt": "hola"}), "grok-4.5").unwrap();
        assert!(body.get("system").is_none());
        assert!(body.get("temperature").is_none());
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn rejects_models_outside_the_grok_allowlist() {
        let error = build_request_body(
            &json!({"prompt": "hola", "model": "gpt-5.6-sol"}),
            "grok-4.5",
        )
        .expect_err("must reject");
        assert!(error.contains("Modelo no soportado"));
    }

    #[test]
    fn rejects_out_of_range_numbers() {
        assert!(build_request_body(&json!({"prompt": "a", "temperature": 9}), "grok-4.5").is_err());
        assert!(build_request_body(&json!({"prompt": "a", "max_tokens": 0}), "grok-4.5").is_err());
        assert!(
            build_request_body(&json!({"prompt": "a", "max_tokens": "muchos"}), "grok-4.5")
                .is_err()
        );
    }

    #[test]
    fn usage_travels_in_its_own_block_after_the_text() {
        let response = call("generate", json!({"prompt": "hola"}));
        let content = response["result"]["content"].as_array().unwrap();

        assert_eq!(content.len(), 2);
        // The text block must stay free of accounting noise.
        assert!(!content[0]["text"].as_str().unwrap().contains("[uso]"));
        let usage = content[1]["text"].as_str().unwrap();
        assert!(usage.contains("entrada=248"), "{usage}");
        assert!(usage.contains("salida=825"), "{usage}");
        assert_eq!(response["result"]["isError"], false);
    }

    #[test]
    fn a_truncated_generation_says_so_in_the_usage_line() {
        let generation = Generation {
            text: "escena a medio".into(),
            input_tokens: Some(10),
            output_tokens: Some(4096),
            stop_reason: Some("max_tokens".into()),
        };
        let usage = generation.usage_line().unwrap();
        assert!(usage.contains("truncada"), "{usage}");
    }

    #[test]
    fn usage_block_is_omitted_when_the_proxy_reports_nothing() {
        let generation = Generation {
            text: "texto".into(),
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
            "content": [{"type": "text", "text": "hola"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 34}
        });
        let generation = extract_generation(&payload).unwrap();
        assert_eq!(generation.text, "hola");
        assert_eq!(generation.input_tokens, Some(12));
        assert_eq!(generation.output_tokens, Some(34));
        assert_eq!(generation.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn extracts_and_joins_text_blocks_only() {
        let payload = json!({"content": [
            {"type": "thinking", "text": "ignorame"},
            {"type": "text", "text": "Escena 1. "},
            {"type": "text", "text": "Marta entra."}
        ]});
        assert_eq!(
            extract_generation(&payload).unwrap().text,
            "Escena 1. Marta entra."
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
