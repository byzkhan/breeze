use async_trait::async_trait;
use serde_json::{Value, json};

use crate::provider::ToolDef;
use crate::util::truncate_output;

use super::Tool;

/// Patterns that require user permission before execution.
const DANGEROUS_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -r /",
    "sudo ",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "mkfs",
    "> /dev/",
    "dd if=",
    "chmod -R 777",
    "curl | sh",
    "curl | bash",
    "wget | sh",
    "wget | bash",
];

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash".to_string(),
            description: "Run a shell command. Use for git, build, test, search (grep/find/rg), install, and any terminal action.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to execute" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (default 120, max 300)" }
                },
                "required": ["command"]
            }),
        }
    }

    fn requires_permission(&self, input: &Value) -> Option<String> {
        let cmd = input["command"].as_str().unwrap_or("");
        for pattern in DANGEROUS_PATTERNS {
            if cmd.contains(pattern) {
                return Some(format!("{}", cmd));
            }
        }
        None
    }

    async fn execute(&self, input: &Value, cwd: &str) -> (String, bool) {
        let command = match input["command"].as_str() {
            Some(c) => c,
            None => return ("Error: missing 'command' parameter".to_string(), false),
        };

        let timeout_secs = input["timeout"]
            .as_u64()
            .unwrap_or(120)
            .min(300)
            .max(1);

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

        let child_result = tokio::process::Command::new(&shell)
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();

        let child = match child_result {
            Ok(c) => c,
            Err(e) => return (format!("Failed to execute: {e}"), false),
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await;

        let (mut output, success) = match result {
            Err(_) => (
                format!("Command timed out after {timeout_secs}s (use & for long-running processes)"),
                false,
            ),
            Ok(Err(e)) => (format!("Failed to execute: {e}"), false),
            Ok(Ok(res)) => {
                let stdout = String::from_utf8_lossy(&res.stdout);
                let stderr = String::from_utf8_lossy(&res.stderr);
                let exit_code = res.status.code().unwrap_or(-1);
                let mut out = String::new();
                if !stdout.is_empty() {
                    out.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str("[stderr]\n");
                    out.push_str(&stderr);
                }
                if out.is_empty() {
                    out.push_str("(no output)");
                }
                out.push_str(&format!("\n[exit code: {exit_code}]"));
                (out, exit_code == 0)
            }
        };

        truncate_output(&mut output, 16000);
        (output, success)
    }
}
