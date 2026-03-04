pub mod anthropic;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

// ── Message types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

// ── Stream events ──────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolUseStart { id: String, name: String },
    ToolInputDelta(String),
    ToolUseEnd { id: String, name: String, input: String },
    Usage { input_tokens: u64, output_tokens: u64 },
    Done { stop_reason: StopReason },
    Error(String),
    RetryAttempt { attempt: u32, max_retries: u32, delay_secs: u64, reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

// ── Tool definition ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// ── Provider trait ─────────────────────────────────────────────

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum ProviderError {
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("Connection failed: {0}")]
    Connection(String),
    #[error("Timed out after all retries")]
    RetriesExhausted,
}

#[async_trait]
#[allow(dead_code)]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<mpsc::Receiver<StreamEvent>, ProviderError>;
}
