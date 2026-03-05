use async_trait::async_trait;
use serde_json::{Value, json};

use crate::provider::ToolDef;
use crate::util::{resolve_path, truncate_output};

use super::Tool;

// ── read_file ──────────────────────────────────────────────────

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read_file".to_string(),
            description: "Read a file with line numbers. ALWAYS read before editing. Use offset/limit for large files.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative to CWD or absolute)" },
                    "offset": { "type": "integer", "description": "Starting line (0-indexed, default 0)" },
                    "limit": { "type": "integer", "description": "Max lines to read (default 200)" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, input: &Value, cwd: &str) -> (String, bool) {
        let path = match input["path"].as_str() {
            Some(p) => p,
            None => return ("Error: missing 'path' parameter".to_string(), false),
        };
        let offset = input["offset"].as_u64().map(|v| v as u32);
        let limit = input["limit"].as_u64().map(|v| v as u32);

        let resolved = match resolve_path(cwd, path) {
            Ok(p) => p,
            Err(e) => return (e, false),
        };
        let content = match std::fs::read_to_string(&resolved) {
            Ok(c) => c,
            Err(e) => return (format!("Error reading {}: {e}", resolved.display()), false),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = offset.unwrap_or(0) as usize;
        let count = limit.unwrap_or(200) as usize;
        let end = start.saturating_add(count).min(total);

        if start >= total {
            return (
                format!(
                    "{} has {total} lines, offset {start} is past end",
                    resolved.display()
                ),
                false,
            );
        }

        let mut output = format!("# {path} ({total} lines)\n");
        for (i, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{:>4} | {}\n", start + i + 1, line));
        }
        if end < total {
            output.push_str(&format!(
                "... ({} more lines, use offset={end} to continue)\n",
                total - end
            ));
        }

        truncate_output(&mut output, 8000);
        (output, true)
    }
}

// ── write_file ─────────────────────────────────────────────────

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write_file".to_string(),
            description: "Create a new file or completely overwrite an existing one. Auto-creates parent directories.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative to CWD or absolute)" },
                    "content": { "type": "string", "description": "Full file content to write" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, input: &Value, cwd: &str) -> (String, bool) {
        let path = match input["path"].as_str() {
            Some(p) => p,
            None => return ("Error: missing 'path' parameter".to_string(), false),
        };
        let content = match input["content"].as_str() {
            Some(c) => c,
            None => return ("Error: missing 'content' parameter".to_string(), false),
        };

        let resolved = match resolve_path(cwd, path) {
            Ok(p) => p,
            Err(e) => return (e, false),
        };

        if let Some(parent) = resolved.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return (
                        format!("Error creating directory {}: {e}", parent.display()),
                        false,
                    );
                }
            }
        }

        match std::fs::write(&resolved, content) {
            Ok(_) => {
                let line_count = content.lines().count();
                (
                    format!("Wrote {} ({line_count} lines)", resolved.display()),
                    true,
                )
            }
            Err(e) => (format!("Error writing {}: {e}", resolved.display()), false),
        }
    }
}

// ── edit_file ──────────────────────────────────────────────────

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit_file".to_string(),
            description: "Find and replace text in a file. old_string must appear exactly once — include enough context lines to be unique.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative to CWD or absolute)" },
                    "old_string": { "type": "string", "description": "Exact text to find (must appear exactly once)" },
                    "new_string": { "type": "string", "description": "Replacement text" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, input: &Value, cwd: &str) -> (String, bool) {
        let path = match input["path"].as_str() {
            Some(p) => p,
            None => return ("Error: missing 'path' parameter".to_string(), false),
        };
        let old_string = match input["old_string"].as_str() {
            Some(s) => s,
            None => return ("Error: missing 'old_string' parameter".to_string(), false),
        };
        let new_string = match input["new_string"].as_str() {
            Some(s) => s,
            None => return ("Error: missing 'new_string' parameter".to_string(), false),
        };

        let resolved = match resolve_path(cwd, path) {
            Ok(p) => p,
            Err(e) => return (e, false),
        };
        let content = match std::fs::read_to_string(&resolved) {
            Ok(c) => c,
            Err(e) => return (format!("Error reading {}: {e}", resolved.display()), false),
        };

        let count = content.matches(old_string).count();
        if count == 0 {
            return (
                format!(
                    "Error: old_string not found in {}. Use read_file to see current content.",
                    resolved.display()
                ),
                false,
            );
        }
        if count > 1 {
            return (
                format!(
                    "Error: old_string appears {count} times in {}. Include more surrounding context to make it unique.",
                    resolved.display()
                ),
                false,
            );
        }

        let new_content = content.replacen(old_string, new_string, 1);
        match std::fs::write(&resolved, &new_content) {
            Ok(_) => (format!("Applied edit to {}", resolved.display()), true),
            Err(e) => (format!("Error writing {}: {e}", resolved.display()), false),
        }
    }
}
