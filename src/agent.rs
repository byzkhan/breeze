use std::collections::VecDeque;
use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::provider::{ContentBlock, LlmProvider, Message, Role, StopReason, StreamEvent};
use crate::tools::ToolRegistry;
use crate::ui::Ui;

const MAX_ITERATIONS: u32 = 50;
const OVERALL_TIMEOUT_SECS: u64 = 600;

pub struct AgentState {
    pub messages: Vec<Message>,
    pub todo: String,
    pub recent_commands: VecDeque<String>,
    pub recent_edits: VecDeque<String>,
    pub cwd: String,
    pub iteration: u32,
}

impl AgentState {
    pub fn new(cwd: String) -> Self {
        Self {
            messages: Vec::new(),
            todo: String::new(),
            recent_commands: VecDeque::with_capacity(6),
            recent_edits: VecDeque::with_capacity(6),
            cwd,
            iteration: 0,
        }
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.todo.clear();
        self.recent_commands.clear();
        self.recent_edits.clear();
        self.iteration = 0;
    }
}

pub struct Agent {
    pub state: AgentState,
    provider: Box<dyn LlmProvider>,
    tools: ToolRegistry,
    system_prompt: String,
}

impl Agent {
    pub fn new(
        cwd: String,
        provider: Box<dyn LlmProvider>,
        tools: ToolRegistry,
    ) -> Self {
        let system_prompt = crate::prompts::default_prompt(&cwd);
        Self {
            state: AgentState::new(cwd),
            provider,
            tools,
            system_prompt,
        }
    }

    pub fn new_with_prompt(
        cwd: String,
        provider: Box<dyn LlmProvider>,
        tools: ToolRegistry,
        system_prompt: String,
    ) -> Self {
        Self {
            state: AgentState::new(cwd),
            provider,
            tools,
            system_prompt,
        }
    }

    pub fn clear(&mut self) {
        self.state.clear();
    }

    /// Run the agent loop for a user message. Returns the accumulated text response.
    pub async fn run(&mut self, message: &str, ui: &mut Ui) -> Result<String> {
        // Push user message
        self.state.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: message.to_string(),
            }],
        });
        self.state.iteration = 0;

        let start = Instant::now();
        let mut full_text = String::new();

        loop {
            if self.state.iteration >= MAX_ITERATIONS {
                let warn = format!(
                    "\n\nReached maximum iteration limit ({MAX_ITERATIONS} turns). The task may be incomplete."
                );
                full_text.push_str(&warn);
                ui.print_warning(&warn);
                break;
            }
            if start.elapsed().as_secs() > OVERALL_TIMEOUT_SECS {
                let warn = "\n\nAgent session timed out after 10 minutes.";
                full_text.push_str(warn);
                ui.print_error(warn);
                break;
            }

            self.state.iteration += 1;

            // Compress old observations before API call
            compress_observations(&mut self.state.messages);

            // Start spinner
            let mut spinner = Some(ui.start_spinner(self.state.iteration, MAX_ITERATIONS));

            // Call provider
            let tool_defs = self.tools.definitions();
            let mut rx = self
                .provider
                .stream(
                    &self.system_prompt,
                    &self.state.messages,
                    &tool_defs,
                    16384,
                )
                .await?;

            // Process stream events
            let mut current_text = String::new();
            let mut tool_calls: Vec<(String, String, String)> = vec![]; // (id, name, input_json)
            let mut stop_reason = StopReason::EndTurn;
            let mut current_tool_name = String::new();
            let mut total_input_tokens: u64 = 0;
            let mut total_output_tokens: u64 = 0;

            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::TextDelta(text) => {
                        if let Some(s) = spinner.take() {
                            ui.stop_spinner(s);
                        }
                        ui.print_text_delta(&text);
                        current_text.push_str(&text);
                    }
                    StreamEvent::ThinkingDelta(_) => {
                        // Let spinner keep showing "Thinking..."
                    }
                    StreamEvent::ToolUseStart { id: _, name } => {
                        // Update spinner message before stopping for bash
                        if let Some(ref s) = spinner {
                            let verb = match name.as_str() {
                                "write_file" => "Writing file...",
                                "edit_file" => "Editing file...",
                                "read_file" => "Reading file...",
                                "bash" => "Running command...",
                                _ => "Working...",
                            };
                            s.set_message(verb);
                        }
                        if let Some(s) = spinner.take() {
                            ui.stop_spinner(s);
                        }
                        current_tool_name = name.clone();
                        ui.tool_use_start(&name);
                    }
                    StreamEvent::ToolInputDelta(chunk) => {
                        ui.tool_input_delta(&current_tool_name, &chunk);
                    }
                    StreamEvent::ToolUseEnd { id, name, input } => {
                        ui.tool_use_complete(&name, &input);
                        tool_calls.push((id, name, input));
                    }
                    StreamEvent::Usage { input_tokens, output_tokens } => {
                        total_input_tokens += input_tokens;
                        total_output_tokens += output_tokens;
                    }
                    StreamEvent::Done { stop_reason: sr } => {
                        stop_reason = sr;
                    }
                    StreamEvent::Error(e) => {
                        if let Some(s) = spinner.take() {
                            ui.stop_spinner(s);
                        }
                        ui.print_error(&e);
                        bail!("{}", e);
                    }
                    StreamEvent::RetryAttempt {
                        attempt,
                        max_retries,
                        delay_secs,
                        reason,
                    } => {
                        ui.print_retry(attempt, max_retries, delay_secs, &reason);
                    }
                }
            }

            if let Some(s) = spinner.take() {
                ui.stop_spinner(s);
            }

            // Ensure newline after streamed text
            if !current_text.is_empty() {
                ui.finish_text();
            }

            // Show token usage for this turn
            if total_input_tokens > 0 || total_output_tokens > 0 {
                ui.print_usage(total_input_tokens, total_output_tokens);
            }

            full_text.push_str(&current_text);

            // Build assistant content blocks
            let mut assistant_content: Vec<ContentBlock> = vec![];
            if !current_text.is_empty() {
                assistant_content.push(ContentBlock::Text {
                    text: current_text,
                });
            }
            for (id, name, input_json) in &tool_calls {
                let input: Value = serde_json::from_str(input_json).unwrap_or(Value::Object(Default::default()));
                assistant_content.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input,
                });
            }

            if assistant_content.is_empty() {
                break;
            }

            self.state.messages.push(Message {
                role: Role::Assistant,
                content: assistant_content,
            });

            // Handle tool calls
            if stop_reason == StopReason::ToolUse && !tool_calls.is_empty() {
                let mut tool_results: Vec<ContentBlock> = vec![];

                for (id, name, input_json) in &tool_calls {
                    let input: Value =
                        serde_json::from_str(input_json).unwrap_or(Value::Object(Default::default()));

                    // Check permission
                    if let Some(tool) = self.tools.get(name) {
                        if let Some(desc) = tool.requires_permission(&input) {
                            if !ui.ask_permission(&desc) {
                                tool_results.push(ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: "User denied permission for this action.".to_string(),
                                    is_error: true,
                                });
                                continue;
                            }
                        }
                    }

                    // Execute
                    let (mut output, success) = if let Some(tool) = self.tools.get(name) {
                        tool.execute(&input, &self.state.cwd).await
                    } else {
                        (format!("Error: unknown tool '{name}'"), false)
                    };

                    // Loop detection
                    if name == "bash" {
                        let cmd = input["command"].as_str().unwrap_or("").to_string();
                        if let Some(warning) = detect_loop(
                            &self.state.recent_commands,
                            &self.state.recent_edits,
                            "bash",
                            &cmd,
                        ) {
                            output.push('\n');
                            output.push_str(&warning);
                        }
                        self.state.recent_commands.push_back(cmd);
                        if self.state.recent_commands.len() > 5 {
                            self.state.recent_commands.pop_front();
                        }
                    } else if name == "edit_file" {
                        let path = input["path"].as_str().unwrap_or("").to_string();
                        if let Some(warning) = detect_loop(
                            &self.state.recent_commands,
                            &self.state.recent_edits,
                            "edit_file",
                            &path,
                        ) {
                            output.push('\n');
                            output.push_str(&warning);
                        }
                        self.state.recent_edits.push_back(path);
                        if self.state.recent_edits.len() > 5 {
                            self.state.recent_edits.pop_front();
                        }
                    }

                    ui.tool_result(&name, &output, success);

                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: output,
                        is_error: !success,
                    });
                }

                // Add system reminder
                let remaining = MAX_ITERATIONS.saturating_sub(self.state.iteration);
                let reminder = build_system_reminder(&self.state.todo, remaining);
                tool_results.push(ContentBlock::Text { text: reminder });

                self.state.messages.push(Message {
                    role: Role::User,
                    content: tool_results,
                });

                continue;
            }

            // end_turn or no tool calls — done
            break;
        }

        Ok(full_text)
    }
}

// ── Helper functions ───────────────────────────────────────────

/// Compress older tool_result observations to save context.
/// Keeps last 5 tool results at full detail, compresses older ones.
fn compress_observations(messages: &mut Vec<Message>) {
    let mut tool_result_indices: Vec<(usize, usize)> = vec![];
    for (msg_idx, msg) in messages.iter().enumerate() {
        for (block_idx, block) in msg.content.iter().enumerate() {
            if matches!(block, ContentBlock::ToolResult { .. }) {
                tool_result_indices.push((msg_idx, block_idx));
            }
        }
    }

    if tool_result_indices.len() <= 5 {
        return;
    }

    let to_compress = tool_result_indices.len() - 5;
    for &(msg_idx, block_idx) in &tool_result_indices[..to_compress] {
        if let Some(ContentBlock::ToolResult { content, .. }) =
            messages[msg_idx].content.get_mut(block_idx)
        {
            let lines: Vec<&str> = content.lines().collect();
            if lines.len() > 40 {
                let mut compressed = String::new();
                for line in &lines[..20] {
                    compressed.push_str(line);
                    compressed.push('\n');
                }
                compressed.push_str(&format!("... ({} lines omitted)\n", lines.len() - 40));
                for line in &lines[lines.len() - 20..] {
                    compressed.push_str(line);
                    compressed.push('\n');
                }
                *content = compressed;
            }
        }
    }

    // Remove stale system reminder text blocks from older user messages (keep last 3)
    let mut reminder_positions: Vec<(usize, usize)> = vec![];
    for (msg_idx, msg) in messages.iter().enumerate() {
        if msg.role != Role::User {
            continue;
        }
        for (block_idx, block) in msg.content.iter().enumerate() {
            if let ContentBlock::Text { text } = block {
                if text.starts_with("[Reminder:") {
                    reminder_positions.push((msg_idx, block_idx));
                }
            }
        }
    }
    if reminder_positions.len() > 3 {
        let to_remove = reminder_positions.len() - 3;
        for &(msg_idx, block_idx) in reminder_positions[..to_remove].iter().rev() {
            if messages[msg_idx].content.len() > 1 {
                messages[msg_idx].content.remove(block_idx);
            }
        }
    }
}

/// Build a system reminder to inject after tool results.
fn build_system_reminder(todo: &str, iterations_remaining: u32) -> String {
    let mut reminder =
        String::from("[Reminder: Read files before editing. Use edit_file for modifications.");

    if iterations_remaining <= 5 {
        reminder.push_str(&format!(
            " | WARNING: Only {} iterations remaining. Wrap up.",
            iterations_remaining
        ));
    } else {
        reminder.push_str(&format!(" | {} iterations remaining.", iterations_remaining));
    }

    if !todo.is_empty() {
        reminder.push_str(&format!(" | Current plan: {}", todo));
    }

    reminder.push(']');
    reminder
}

/// Detect repeated commands/edits and return a warning if looping.
fn detect_loop(
    recent_commands: &VecDeque<String>,
    recent_edits: &VecDeque<String>,
    tool_type: &str,
    identifier: &str,
) -> Option<String> {
    match tool_type {
        "bash" => {
            let count = recent_commands.iter().filter(|c| c.as_str() == identifier).count();
            if count >= 3 {
                Some(
                    "Warning: You've run this exact command 3+ times. Try a different approach."
                        .to_string(),
                )
            } else {
                None
            }
        }
        "edit_file" => {
            let count = recent_edits.iter().filter(|f| f.as_str() == identifier).count();
            if count >= 3 {
                Some(
                    "Warning: You've edited this file 3+ times. Read the full file first and reconsider your approach."
                        .to_string(),
                )
            } else {
                None
            }
        }
        _ => None,
    }
}

