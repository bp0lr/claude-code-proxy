use std::{collections::VecDeque, convert::Infallible};

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use serde_json::{Value, json};

use crate::{
    provider::{Generation, GenerationBody},
    providers::codex::native::NativeResponseOutcome,
};

use super::{
    MAX_PROVIDER_STREAM_BYTES, MAX_SSE_EVENT_BYTES, OpenAiError, OpenAiSurface,
    response::{
        AnthropicAccumulator, BlockKind, SseEvent, buffered_response, chat_finish_reason,
        normalized_arguments, responses_response,
    },
};

#[derive(Default)]
pub struct SseDecoder {
    pending: BytesMut,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, OpenAiError> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > MAX_SSE_EVENT_BYTES {
            return Err(OpenAiError::invalid(
                "Provider SSE event exceeded the size limit",
                None::<String>,
            ));
        }
        let mut events = Vec::new();
        while let Some((position, delimiter_len)) = find_event_delimiter(&self.pending) {
            let frame = self.pending.split_to(position + delimiter_len);
            let payload = &frame[..position];
            if let Some(event) = parse_frame(payload)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub fn finish(&self) -> Result<(), OpenAiError> {
        if self.pending.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err(OpenAiError::invalid(
                "Provider SSE stream ended with an incomplete event",
                None::<String>,
            ))
        }
    }
}

pub async fn openai_response(
    surface: OpenAiSurface,
    generation: Generation,
    stream: bool,
    include_usage: bool,
    model: String,
) -> Result<Response, OpenAiError> {
    let response_id = match surface {
        OpenAiSurface::ChatCompletions => format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        OpenAiSurface::Responses => format!("resp_{}", uuid::Uuid::new_v4().simple()),
    };
    let created = current_seconds();
    if stream {
        return Ok(streaming_response(
            surface,
            generation.body,
            include_usage,
            response_id,
            model,
            created,
        ));
    }
    let bytes = collect_generation(generation.body).await?;
    let events = decode_all(&bytes)?;
    let value = buffered_response(surface, &events, &response_id, &model, created)?;
    Ok((StatusCode::OK, Json(value)).into_response())
}

async fn collect_generation(body: GenerationBody) -> Result<Bytes, OpenAiError> {
    match body {
        GenerationBody::BufferedSse(bytes) => {
            if bytes.len() > MAX_PROVIDER_STREAM_BYTES {
                Err(OpenAiError::invalid(
                    "Provider response exceeded the size limit",
                    None::<String>,
                ))
            } else {
                Ok(bytes)
            }
        }
        GenerationBody::LiveSse(mut body) => {
            let mut output = BytesMut::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|error| OpenAiError {
                    status: StatusCode::BAD_GATEWAY,
                    kind: "api_error".into(),
                    message: format!("Provider stream read failed: {error}").into(),
                    param: None,
                    code: None,
                    retry_after: None,
                })?;
                if let Ok(data) = frame.into_data() {
                    if output.len().saturating_add(data.len()) > MAX_PROVIDER_STREAM_BYTES {
                        return Err(OpenAiError::invalid(
                            "Provider response exceeded the size limit",
                            None::<String>,
                        ));
                    }
                    output.extend_from_slice(&data);
                }
            }
            Ok(output.freeze())
        }
    }
}

fn decode_all(bytes: &[u8]) -> Result<Vec<SseEvent>, OpenAiError> {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(bytes)?;
    decoder.finish()?;
    Ok(events)
}

fn streaming_response(
    surface: OpenAiSurface,
    body: GenerationBody,
    include_usage: bool,
    response_id: String,
    model: String,
    created: u64,
) -> Response {
    let outcome = NativeResponseOutcome::default();
    let state = StreamState {
        body: match body {
            GenerationBody::BufferedSse(bytes) => Body::from(bytes),
            GenerationBody::LiveSse(body) => body,
        },
        decoder: SseDecoder::default(),
        renderer: Renderer::new(surface, include_usage, response_id, model, created),
        pending: VecDeque::new(),
        finished: false,
        bytes: 0,
        outcome: outcome.clone(),
    };
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        state
            .next()
            .await
            .map(|bytes| (Ok::<Bytes, Infallible>(bytes), state))
    });
    let mut response = (
        [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            (http::header::CACHE_CONTROL, "no-cache"),
            (http::header::CONNECTION, "keep-alive"),
        ],
        Body::from_stream(stream),
    )
        .into_response();
    response.extensions_mut().insert(outcome);
    response
}

struct StreamState {
    body: Body,
    decoder: SseDecoder,
    renderer: Renderer,
    pending: VecDeque<Bytes>,
    finished: bool,
    bytes: usize,
    outcome: NativeResponseOutcome,
}

impl StreamState {
    async fn next(&mut self) -> Option<Bytes> {
        loop {
            if let Some(bytes) = self.pending.pop_front() {
                return Some(bytes);
            }
            if self.finished {
                return None;
            }
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    let Ok(data) = frame.into_data() else {
                        continue;
                    };
                    self.bytes = self.bytes.saturating_add(data.len());
                    if self.bytes > MAX_PROVIDER_STREAM_BYTES {
                        self.fail(OpenAiError::invalid(
                            "Provider response exceeded the size limit",
                            None::<String>,
                        ));
                        continue;
                    }
                    match self.decoder.push(&data) {
                        Ok(events) => {
                            for event in events {
                                match self.renderer.render(&event) {
                                    Ok(frames) => self.pending.extend(frames),
                                    Err(error) => {
                                        self.fail(error);
                                        break;
                                    }
                                }
                            }
                        }
                        Err(error) => self.fail(error),
                    }
                }
                Some(Err(error)) => self.fail(OpenAiError {
                    status: StatusCode::BAD_GATEWAY,
                    kind: "api_error".into(),
                    message: format!("Provider stream read failed: {error}").into(),
                    param: None,
                    code: None,
                    retry_after: None,
                }),
                None => {
                    if let Err(error) = self.decoder.finish() {
                        self.fail(error);
                    } else if !self.renderer.state.stopped {
                        self.fail(OpenAiError::invalid(
                            "Provider stream ended before message_stop",
                            None::<String>,
                        ));
                    } else {
                        self.finished = true;
                    }
                }
            }
        }
    }

    fn fail(&mut self, error: OpenAiError) {
        self.outcome.fail(error.message.to_string());
        self.pending.extend(self.renderer.failure(&error));
        self.finished = true;
    }
}

struct Renderer {
    surface: OpenAiSurface,
    include_usage: bool,
    response_id: String,
    model: String,
    created: u64,
    sequence: u64,
    state: AnthropicAccumulator,
    responses_message_started: bool,
}

impl Renderer {
    fn new(
        surface: OpenAiSurface,
        include_usage: bool,
        response_id: String,
        model: String,
        created: u64,
    ) -> Self {
        Self {
            surface,
            include_usage,
            response_id,
            model,
            created,
            sequence: 0,
            state: AnthropicAccumulator::default(),
            responses_message_started: false,
        }
    }

    fn render(&mut self, event: &SseEvent) -> Result<Vec<Bytes>, OpenAiError> {
        self.state.apply(event)?;
        match self.surface {
            OpenAiSurface::ChatCompletions => self.render_chat(event),
            OpenAiSurface::Responses => self.render_responses(event),
        }
    }

    fn render_chat(&self, event: &SseEvent) -> Result<Vec<Bytes>, OpenAiError> {
        let kind = event
            .data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut out = Vec::new();
        match kind {
            "message_start" => out.push(chat_data(json!({
                "id":self.response_id,
                "object":"chat.completion.chunk",
                "created":self.created,
                "model":self.model,
                "choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null,"logprobs":null}],
            }))),
            "content_block_start" => {
                let index = event.data.get("index").and_then(Value::as_u64).unwrap_or_default();
                if let Some(block) = self.state.blocks.iter().rev().find(|block| block.index == index as usize)
                    && let BlockKind::Tool { id, name, .. } = &block.kind
                {
                    out.push(chat_data(json!({
                        "id":self.response_id,
                        "object":"chat.completion.chunk",
                        "created":self.created,
                        "model":self.model,
                        "choices":[{"index":0,"delta":{"tool_calls":[{"index":index,"id":id,"type":"function","function":{"name":name,"arguments":""}}]},"finish_reason":null,"logprobs":null}],
                    })));
                }
            }
            "content_block_delta" => {
                let index = event.data.get("index").and_then(Value::as_u64).unwrap_or_default();
                let delta = event.data.get("delta").unwrap_or(&Value::Null);
                let payload = match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => json!({"content":delta.get("text").and_then(Value::as_str).unwrap_or_default()}),
                    Some("thinking_delta") => json!({"reasoning_content":delta.get("thinking").and_then(Value::as_str).unwrap_or_default()}),
                    Some("input_json_delta") => json!({"tool_calls":[{"index":index,"function":{"arguments":delta.get("partial_json").and_then(Value::as_str).unwrap_or_default()}}]}),
                    Some("signature_delta") => return Ok(out),
                    _ => return Err(OpenAiError::invalid("Unsupported provider content delta", None::<String>)),
                };
                out.push(chat_data(json!({
                    "id":self.response_id,
                    "object":"chat.completion.chunk",
                    "created":self.created,
                    "model":self.model,
                    "choices":[{"index":0,"delta":payload,"finish_reason":null,"logprobs":null}],
                })));
            }
            "message_delta" => {
                let mut chunk = json!({
                    "id":self.response_id,
                    "object":"chat.completion.chunk",
                    "created":self.created,
                    "model":self.model,
                    "choices":[{"index":0,"delta":{},"finish_reason":chat_finish_reason(self.state.stop_reason.as_deref()),"logprobs":null}],
                });
                if self.include_usage {
                    chunk["usage"] = self.state.usage.chat_value();
                }
                out.push(chat_data(chunk));
            }
            "message_stop" => out.push(Bytes::from_static(b"data: [DONE]\n\n")),
            _ => {}
        }
        Ok(out)
    }

    fn render_responses(&mut self, event: &SseEvent) -> Result<Vec<Bytes>, OpenAiError> {
        let kind = event
            .data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut out = Vec::new();
        match kind {
            "message_start" => {
                let response =
                    response_shell(&self.response_id, &self.model, self.created, "in_progress");
                out.push(self.responses_event("response.created", json!({"response":response})));
                out.push(
                    self.responses_event("response.in_progress", json!({"response":response})),
                );
            }
            "content_block_start" => {
                let index = event
                    .data
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                let block = self
                    .state
                    .blocks
                    .iter()
                    .rev()
                    .find(|block| block.index == index)
                    .ok_or_else(|| {
                        OpenAiError::invalid("Provider content block is missing", None::<String>)
                    })?;
                match &block.kind {
                    BlockKind::Text { .. } => {
                        let item_id = self.message_item_id();
                        if !self.responses_message_started {
                            out.push(self.responses_event("response.output_item.added", json!({
                                "output_index":index,
                                "item":{"id":item_id,"type":"message","role":"assistant","status":"in_progress","content":[]},
                            })));
                            self.responses_message_started = true;
                        }
                        out.push(self.responses_event("response.content_part.added", json!({
                            "item_id":item_id,"output_index":index,"content_index":0,
                            "part":{"type":"output_text","text":"","annotations":[]},
                        })));
                    }
                    BlockKind::Thinking { .. } => out.push(self.responses_event("response.output_item.added", json!({
                        "output_index":index,
                        "item":{"id":self.block_item_id("rs", index),"type":"reasoning","status":"in_progress","summary":[]},
                    }))),
                    BlockKind::Tool { id, name, .. } => out.push(self.responses_event("response.output_item.added", json!({
                        "output_index":index,
                        "item":{"id":self.block_item_id("fc", index),"type":"function_call","call_id":id,"name":name,"arguments":"","status":"in_progress"},
                    }))),
                }
            }
            "content_block_delta" => {
                let index = event
                    .data
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                let delta = event.data.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => out.push(self.responses_event("response.output_text.delta", json!({
                        "item_id":self.message_item_id(),"output_index":index,"content_index":0,
                        "delta":delta.get("text").and_then(Value::as_str).unwrap_or_default(),"logprobs":[],
                    }))),
                    Some("thinking_delta") => out.push(self.responses_event("response.reasoning_summary_text.delta", json!({
                        "item_id":self.block_item_id("rs", index),"output_index":index,"summary_index":0,
                        "delta":delta.get("thinking").and_then(Value::as_str).unwrap_or_default(),
                    }))),
                    Some("input_json_delta") => out.push(self.responses_event("response.function_call_arguments.delta", json!({
                        "item_id":self.block_item_id("fc", index),"output_index":index,
                        "delta":delta.get("partial_json").and_then(Value::as_str).unwrap_or_default(),
                    }))),
                    Some("signature_delta") => {}
                    _ => return Err(OpenAiError::invalid("Unsupported provider content delta", None::<String>)),
                }
            }
            "content_block_stop" => {
                let index = event
                    .data
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                let block = self
                    .state
                    .blocks
                    .iter()
                    .rev()
                    .find(|block| block.index == index)
                    .cloned()
                    .ok_or_else(|| {
                        OpenAiError::invalid("Provider content block is missing", None::<String>)
                    })?;
                match block.kind {
                    BlockKind::Text { text } => {
                        out.push(self.responses_event("response.output_text.done", json!({
                            "item_id":self.message_item_id(),"output_index":index,"content_index":0,"text":text,"logprobs":[],
                        })));
                        out.push(self.responses_event("response.content_part.done", json!({
                            "item_id":self.message_item_id(),"output_index":index,"content_index":0,
                            "part":{"type":"output_text","text":text,"annotations":[]},
                        })));
                        out.push(self.responses_event("response.output_item.done", json!({
                            "output_index":index,
                            "item":{"id":self.message_item_id(),"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]},
                        })));
                    }
                    BlockKind::Thinking { text } => {
                        out.push(self.responses_event("response.reasoning_summary_text.done", json!({
                            "item_id":self.block_item_id("rs", index),"output_index":index,"summary_index":0,"text":text,
                        })));
                        out.push(self.responses_event("response.output_item.done", json!({
                            "output_index":index,
                            "item":{"id":self.block_item_id("rs", index),"type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":text}]},
                        })));
                    }
                    BlockKind::Tool {
                        id,
                        name,
                        arguments,
                    } => {
                        let arguments = normalized_arguments(&arguments);
                        out.push(self.responses_event("response.function_call_arguments.done", json!({
                            "item_id":self.block_item_id("fc", index),"output_index":index,"arguments":arguments,
                        })));
                        out.push(self.responses_event("response.output_item.done", json!({
                            "output_index":index,
                            "item":{"id":self.block_item_id("fc", index),"type":"function_call","call_id":id,"name":name,"arguments":arguments,"status":"completed"},
                        })));
                    }
                }
            }
            "message_stop" => {
                let response =
                    responses_response(&self.state, &self.response_id, &self.model, self.created);
                out.push(self.responses_event("response.completed", json!({"response":response})));
            }
            _ => {}
        }
        Ok(out)
    }

    fn failure(&mut self, error: &OpenAiError) -> Vec<Bytes> {
        match self.surface {
            OpenAiSurface::ChatCompletions => vec![chat_data(json!({
                "error":{"message":error.message,"type":error.kind,"param":error.param,"code":error.code}
            }))],
            OpenAiSurface::Responses => {
                let mut response =
                    response_shell(&self.response_id, &self.model, self.created, "failed");
                response["error"] = json!({"code":error.code,"message":error.message});
                vec![self.responses_event("response.failed", json!({"response":response}))]
            }
        }
    }

    fn responses_event(&mut self, kind: &str, fields: Value) -> Bytes {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        let mut value = fields.as_object().cloned().unwrap_or_default();
        value.insert("type".to_string(), Value::String(kind.to_string()));
        value.insert("sequence_number".to_string(), json!(sequence));
        named_sse(kind, Value::Object(value))
    }

    fn message_item_id(&self) -> String {
        format!("msg_{}", self.response_id.trim_start_matches("resp_"))
    }

    fn block_item_id(&self, prefix: &str, index: usize) -> String {
        format!(
            "{prefix}_{}_{index}",
            self.response_id.trim_start_matches("resp_")
        )
    }
}

fn chat_data(value: Value) -> Bytes {
    Bytes::from(format!("data: {}\n\n", value))
}

fn named_sse(kind: &str, value: Value) -> Bytes {
    Bytes::from(format!("event: {kind}\ndata: {value}\n\n"))
}

fn response_shell(id: &str, model: &str, created: u64, status: &str) -> Value {
    json!({
        "id":id,
        "object":"response",
        "created_at":created,
        "status":status,
        "model":model,
        "output":[],
        "parallel_tool_calls":false,
        "error":null,
        "incomplete_details":null,
        "usage":null,
    })
}

fn parse_frame(frame: &[u8]) -> Result<Option<SseEvent>, OpenAiError> {
    let text = std::str::from_utf8(frame)
        .map_err(|_| OpenAiError::invalid("Provider SSE event is not UTF-8", None::<String>))?;
    let mut event = None;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    let data = data.join("\n");
    if data == "[DONE]" {
        return Ok(None);
    }
    let data = serde_json::from_str(&data).map_err(|error| {
        OpenAiError::invalid(
            format!("Provider SSE event contains invalid JSON: {error}"),
            None::<String>,
        )
    })?;
    Ok(Some(SseEvent { event, data }))
}

fn find_event_delimiter(bytes: &[u8]) -> Option<(usize, usize)> {
    bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, 2))
        .or_else(|| {
            bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| (position, 4))
        })
}

fn current_seconds() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn decoder_handles_fragmented_and_batched_events() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"event: message_start\nda")
                .unwrap()
                .is_empty()
        );
        let events = decoder
            .push(b"ta: {\"type\":\"message_start\",\"message\":{}}\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\n")
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        decoder.finish().unwrap();
    }

    #[tokio::test]
    async fn chat_stream_emits_tool_deltas_usage_and_done() {
        let input = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":2}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"lookup\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let response = streaming_response(
            OpenAiSurface::ChatCompletions,
            GenerationBody::BufferedSse(Bytes::from_static(input.as_bytes())),
            true,
            "chatcmpl_test".into(),
            "kimi-k2.6".into(),
            1,
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("tool_calls"));
        assert!(text.contains("\"finish_reason\":\"tool_calls\""));
        assert!(text.contains("\"total_tokens\":3"));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn responses_stream_numbers_events_and_completes() {
        let input = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let response = streaming_response(
            OpenAiSurface::Responses,
            GenerationBody::BufferedSse(Bytes::from_static(input.as_bytes())),
            false,
            "resp_test".into(),
            "grok-4.5".into(),
            1,
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("event: response.created"));
        assert!(text.contains("event: response.output_text.delta"));
        assert!(text.contains("event: response.completed"));
        assert!(text.contains("\"sequence_number\":0"));
    }
}
