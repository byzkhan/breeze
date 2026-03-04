use super::{
    ContentBlock, LlmProvider, Message, ProviderError, Role, StopReason, StreamEvent, ToolDef,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct AnthropicProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .build()
            .expect("failed to build HTTP client");
        Self {
            api_key,
            model,
            client,
        }
    }

    #[allow(dead_code)]
    pub fn set_model(&mut self, model: String) {
        self.model = model;
    }
}

// ── Helpers ────────────────────────────────────────────────────

fn parse_api_error(body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(msg) = json["error"]["message"].as_str() {
            return msg.to_string();
        }
        if let Some(msg) = json["message"].as_str() {
            return msg.to_string();
        }
    }
    body.chars().take(200).collect()
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 503 | 529)
}

fn parse_retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Serialize our Message types into the Anthropic wire format.
fn serialize_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|msg| {
            let role = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content: Vec<serde_json::Value> = msg
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => json!({"type": "text", "text": text}),
                    ContentBlock::ToolUse { id, name, input } => {
                        json!({"type": "tool_use", "id": id, "name": name, "input": input})
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
                    }
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect()
}

/// Serialize tool definitions for the API.
fn serialize_tools(tools: &[ToolDef]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect()
}

// ── SSE Parser ─────────────────────────────────────────────────

async fn parse_sse_stream(
    response: reqwest::Response,
    tx: mpsc::Sender<StreamEvent>,
) {
    let mut stream = response.bytes_stream();
    let mut sse_bytes: Vec<u8> = Vec::new();
    let mut current_block_type = String::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();
    let mut current_tool_json = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                return;
            }
        };
        sse_bytes.extend_from_slice(&chunk);

        while let Some(newline_pos) = sse_bytes.iter().position(|&b| b == b'\n') {
            let mut line_bytes = sse_bytes[..newline_pos].to_vec();
            sse_bytes = sse_bytes[newline_pos + 1..].to_vec();
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8_lossy(&line_bytes).to_string();

            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    continue;
                }
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    let event_type = event["type"].as_str().unwrap_or("");
                    match event_type {
                        "content_block_start" => {
                            let block_type =
                                event["content_block"]["type"].as_str().unwrap_or("");
                            current_block_type = block_type.to_string();
                            if block_type == "tool_use" {
                                current_tool_id = event["content_block"]["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                current_tool_name = event["content_block"]["name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                current_tool_json = String::new();
                                let _ = tx
                                    .send(StreamEvent::ToolUseStart {
                                        id: current_tool_id.clone(),
                                        name: current_tool_name.clone(),
                                    })
                                    .await;
                            }
                        }
                        "content_block_delta" => {
                            if current_block_type == "text" {
                                if let Some(text) = event["delta"]["text"].as_str() {
                                    let _ =
                                        tx.send(StreamEvent::TextDelta(text.to_string())).await;
                                }
                            } else if current_block_type == "tool_use" {
                                if let Some(json_chunk) =
                                    event["delta"]["partial_json"].as_str()
                                {
                                    current_tool_json.push_str(json_chunk);
                                    let _ = tx
                                        .send(StreamEvent::ToolInputDelta(
                                            json_chunk.to_string(),
                                        ))
                                        .await;
                                }
                            }
                        }
                        "content_block_stop" => {
                            if current_block_type == "tool_use" {
                                let _ = tx
                                    .send(StreamEvent::ToolUseEnd {
                                        id: current_tool_id.clone(),
                                        name: current_tool_name.clone(),
                                        input: current_tool_json.clone(),
                                    })
                                    .await;
                            }
                            current_block_type.clear();
                        }
                        "message_delta" => {
                            if let Some(sr) = event["delta"]["stop_reason"].as_str() {
                                let stop = match sr {
                                    "tool_use" => StopReason::ToolUse,
                                    "max_tokens" => StopReason::MaxTokens,
                                    _ => StopReason::EndTurn,
                                };
                                let _ =
                                    tx.send(StreamEvent::Done { stop_reason: stop }).await;
                            }
                        }
                        "error" => {
                            let err_msg = event["error"]["message"]
                                .as_str()
                                .unwrap_or("Unknown stream error");
                            let _ = tx.send(StreamEvent::Error(err_msg.to_string())).await;
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Handle trailing bytes in the SSE buffer
    if !sse_bytes.is_empty() {
        while sse_bytes.last().map_or(false, |&b| b == b'\r' || b == b'\n') {
            sse_bytes.pop();
        }
        let line = String::from_utf8_lossy(&sse_bytes);
        if let Some(data) = line.strip_prefix("data: ") {
            if data != "[DONE]" {
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    match event["type"].as_str() {
                        Some("content_block_delta") => {
                            if current_block_type == "text" {
                                if let Some(text) = event["delta"]["text"].as_str() {
                                    let _ =
                                        tx.send(StreamEvent::TextDelta(text.to_string())).await;
                                }
                            } else if current_block_type == "tool_use" {
                                if let Some(json_chunk) =
                                    event["delta"]["partial_json"].as_str()
                                {
                                    current_tool_json.push_str(json_chunk);
                                    let _ = tx
                                        .send(StreamEvent::ToolInputDelta(
                                            json_chunk.to_string(),
                                        ))
                                        .await;
                                }
                            }
                        }
                        Some("content_block_stop") => {
                            if current_block_type == "tool_use" && !current_tool_id.is_empty() {
                                let _ = tx
                                    .send(StreamEvent::ToolUseEnd {
                                        id: current_tool_id.clone(),
                                        name: current_tool_name.clone(),
                                        input: current_tool_json.clone(),
                                    })
                                    .await;
                            }
                            current_block_type.clear();
                        }
                        Some("message_delta") => {
                            if let Some(sr) = event["delta"]["stop_reason"].as_str() {
                                let stop = match sr {
                                    "tool_use" => StopReason::ToolUse,
                                    "max_tokens" => StopReason::MaxTokens,
                                    _ => StopReason::EndTurn,
                                };
                                let _ =
                                    tx.send(StreamEvent::Done { stop_reason: stop }).await;
                            }
                        }
                        Some("error") => {
                            let err_msg = event["error"]["message"]
                                .as_str()
                                .unwrap_or("Unknown stream error");
                            let _ = tx.send(StreamEvent::Error(err_msg.to_string())).await;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // If a tool_use block was never closed, send it now
    if current_block_type == "tool_use" && !current_tool_id.is_empty() {
        let _ = tx
            .send(StreamEvent::ToolUseEnd {
                id: current_tool_id,
                name: current_tool_name,
                input: current_tool_json,
            })
            .await;
    }
}

// ── Provider implementation ────────────────────────────────────

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<mpsc::Receiver<StreamEvent>, ProviderError> {
        let (tx, rx) = mpsc::channel(256);

        let wire_messages = serialize_messages(messages);
        let wire_tools = serialize_tools(tools);
        let system = system.to_string();
        let model = self.model.clone();
        let api_key = self.api_key.clone();
        let client = self.client.clone();

        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let max_retries: u32 = 5;

            for attempt in 0..=max_retries {
                let body = json!({
                    "model": model,
                    "max_tokens": max_tokens,
                    "stream": true,
                    "system": system,
                    "tools": wire_tools,
                    "messages": wire_messages,
                });

                let res = client
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await;

                match res {
                    Ok(r) => {
                        if r.status().is_success() {
                            parse_sse_stream(r, tx_clone).await;
                            return;
                        }
                        let status = r.status().as_u16();
                        let retry_after = parse_retry_after(&r);
                        let body_text = r.text().await.unwrap_or_default();

                        if is_retryable_status(status) && attempt < max_retries {
                            let delay =
                                retry_after.unwrap_or_else(|| std::cmp::min(1u64 << attempt, 60));
                            let reason = parse_api_error(&body_text);
                            let _ = tx_clone
                                .send(StreamEvent::RetryAttempt {
                                    attempt: attempt + 1,
                                    max_retries,
                                    delay_secs: delay,
                                    reason,
                                })
                                .await;
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            continue;
                        }

                        let msg = parse_api_error(&body_text);
                        let _ = tx_clone
                            .send(StreamEvent::Error(format!("API error ({}): {}", status, msg)))
                            .await;
                        return;
                    }
                    Err(e) => {
                        let is_connection = e.is_connect() || e.is_timeout();
                        if is_connection && attempt < max_retries {
                            let delay = std::cmp::min(1u64 << attempt, 60);
                            let _ = tx_clone
                                .send(StreamEvent::RetryAttempt {
                                    attempt: attempt + 1,
                                    max_retries,
                                    delay_secs: delay,
                                    reason: e.to_string(),
                                })
                                .await;
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            continue;
                        }
                        let _ = tx_clone
                            .send(StreamEvent::Error(format!(
                                "Request failed: {}",
                                e
                            )))
                            .await;
                        return;
                    }
                }
            }

            let _ = tx_clone
                .send(StreamEvent::Error("Retries exhausted".to_string()))
                .await;
        });

        Ok(rx)
    }
}
