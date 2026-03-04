use anyhow::Result;
use std::process::Command;

use crate::agent::Agent;
use crate::checkpoint::Checkpoint;
use crate::config::Config;
use crate::prompts;
use crate::provider::anthropic::AnthropicProvider;
use crate::tools::ToolRegistry;
use crate::ui::Ui;

/// Result of a harness run.
#[allow(dead_code)]
pub enum HarnessResult {
    /// Pipeline completed successfully.
    Success(String),
    /// Pipeline failed; changes may have been rolled back.
    Failed { reason: String, rolled_back: bool },
    /// Simple task — passed through to default agent directly.
    Passthrough(String),
}

/// A parsed subtask from the planner output.
struct Subtask {
    description: String,
    files: String,
}

/// The Planner → Worker → Judge pipeline orchestrator.
pub struct Harness {
    api_key: String,
    model: String,
    cwd: String,
}

impl Harness {
    pub fn new(config: &Config, model: &str, cwd: &str) -> Self {
        Self {
            api_key: config.api_key.clone(),
            model: model.to_string(),
            cwd: cwd.to_string(),
        }
    }

    /// Run the full pipeline for a user message.
    pub async fn run(&self, message: &str, ui: &mut Ui) -> Result<HarnessResult> {
        // Phase 1: Planner
        ui.print_info("[harness] Running planner...");
        let plan_output = self.run_planner(message, ui).await?;

        // Parse complexity
        let complexity = parse_complexity(&plan_output);

        if complexity == Complexity::Simple {
            // Short-circuit: run as default agent
            ui.print_info("[harness] Simple task — running default agent.");
            let result = self.run_default_agent(message, ui).await?;
            return Ok(HarnessResult::Passthrough(result));
        }

        // Parse subtasks
        let subtasks = parse_subtasks(&plan_output);
        if subtasks.is_empty() {
            // Parsing failed — fall back to default agent
            ui.print_info("[harness] Could not parse subtasks — falling back to default agent.");
            let result = self.run_default_agent(message, ui).await?;
            return Ok(HarnessResult::Passthrough(result));
        }

        ui.print_info(&format!(
            "[harness] Plan: {} subtasks. Creating checkpoint...",
            subtasks.len()
        ));

        // Phase 2: Checkpoint
        let checkpoint = Checkpoint::create(&self.cwd).ok();

        // Phase 3: Workers (sequential, one per subtask)
        for (i, subtask) in subtasks.iter().enumerate() {
            ui.print_info(&format!(
                "[harness] Worker {}/{}: {}",
                i + 1,
                subtasks.len(),
                subtask.description
            ));
            if let Err(e) = self.run_worker(subtask, ui).await {
                ui.print_error(&format!("[harness] Worker {} failed: {}", i + 1, e));
                // Rollback on worker failure
                if let Some(cp) = checkpoint {
                    ui.print_info("[harness] Rolling back changes...");
                    let rolled_back = cp.rollback().is_ok();
                    return Ok(HarnessResult::Failed {
                        reason: format!("Worker {} failed: {}", i + 1, e),
                        rolled_back,
                    });
                }
                return Ok(HarnessResult::Failed {
                    reason: format!("Worker {} failed: {}", i + 1, e),
                    rolled_back: false,
                });
            }
        }

        // Phase 4: Auto-verify
        ui.print_info("[harness] Running auto-verification...");
        let verify_output = self.auto_verify();

        // Phase 5: Judge
        ui.print_info("[harness] Running judge...");
        let git_diff = get_git_diff(&self.cwd);
        let judge_input = format!(
            "## Original request\n{}\n\n## Plan\n{}\n\n## Git diff\n```\n{}\n```\n\n## Auto-verification results\n```\n{}\n```",
            message, plan_output, git_diff, verify_output
        );

        let judge_output = self.run_judge(&judge_input, ui).await?;
        let verdict = parse_verdict(&judge_output);

        match verdict {
            Verdict::Pass(reason) => {
                ui.print_info(&format!("[harness] PASS: {}", reason));
                if let Some(cp) = checkpoint {
                    cp.discard();
                }
                Ok(HarnessResult::Success(judge_output))
            }
            Verdict::Fail(reason) => {
                ui.print_error(&format!("[harness] FAIL: {}", reason));
                if let Some(cp) = checkpoint {
                    ui.print_info("[harness] Rolling back changes...");
                    let rolled_back = cp.rollback().is_ok();
                    Ok(HarnessResult::Failed {
                        reason,
                        rolled_back,
                    })
                } else {
                    Ok(HarnessResult::Failed {
                        reason,
                        rolled_back: false,
                    })
                }
            }
        }
    }

    // ── Phase runners ─────────────────────────────────────────

    async fn run_planner(&self, message: &str, ui: &mut Ui) -> Result<String> {
        let provider = Box::new(AnthropicProvider::new(
            self.api_key.clone(),
            self.model.clone(),
        ));
        let tools = ToolRegistry::exploration_registry();
        let prompt = prompts::planner_prompt(&self.cwd);
        let mut agent = Agent::new_with_prompt(self.cwd.clone(), provider, tools, prompt);
        agent.run(message, ui).await
    }

    async fn run_worker(&self, subtask: &Subtask, ui: &mut Ui) -> Result<String> {
        let provider = Box::new(AnthropicProvider::new(
            self.api_key.clone(),
            self.model.clone(),
        ));
        let tools = ToolRegistry::default_registry();
        let prompt = prompts::worker_prompt(&self.cwd);
        let mut agent = Agent::new_with_prompt(self.cwd.clone(), provider, tools, prompt);

        let worker_message = format!(
            "## Subtask\n{}\n\n## Files to touch\n{}",
            subtask.description, subtask.files
        );
        agent.run(&worker_message, ui).await
    }

    async fn run_judge(&self, input: &str, ui: &mut Ui) -> Result<String> {
        let provider = Box::new(AnthropicProvider::new(
            self.api_key.clone(),
            self.model.clone(),
        ));
        let tools = ToolRegistry::exploration_registry();
        let prompt = prompts::judge_prompt(&self.cwd);
        let mut agent = Agent::new_with_prompt(self.cwd.clone(), provider, tools, prompt);
        agent.run(input, ui).await
    }

    async fn run_default_agent(&self, message: &str, ui: &mut Ui) -> Result<String> {
        let provider = Box::new(AnthropicProvider::new(
            self.api_key.clone(),
            self.model.clone(),
        ));
        let tools = ToolRegistry::default_registry();
        let mut agent = Agent::new(self.cwd.clone(), provider, tools);
        agent.run(message, ui).await
    }

    // ── Auto-verification ─────────────────────────────────────

    fn auto_verify(&self) -> String {
        let mut results = String::new();

        // Try cargo check first
        match Command::new("cargo")
            .args(["check", "--message-format=short"])
            .current_dir(&self.cwd)
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if output.status.success() {
                    results.push_str("cargo check: OK\n");
                } else {
                    results.push_str("cargo check: FAILED\n");
                    if !stdout.is_empty() {
                        results.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        results.push_str(&stderr);
                    }
                }
            }
            Err(_) => {
                // Not a Rust project or cargo not available — that's fine
                results.push_str("cargo check: skipped (not available)\n");
            }
        }

        // Try cargo test (blocking — no timeout enforced at this level)
        match Command::new("cargo")
            .args(["test", "--", "--test-threads=1"])
            .current_dir(&self.cwd)
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if output.status.success() {
                    results.push_str("cargo test: OK\n");
                } else {
                    results.push_str("cargo test: FAILED\n");
                    // Truncate test output to avoid bloating the judge context
                    let combined = format!("{}{}", stdout, stderr);
                    let truncated: String = combined.chars().take(2000).collect();
                    results.push_str(&truncated);
                    if combined.len() > 2000 {
                        results.push_str("\n... (truncated)");
                    }
                    results.push('\n');
                }
            }
            Err(_) => {
                results.push_str("cargo test: skipped (not available)\n");
            }
        }

        results
    }
}

// ── Parsing helpers ───────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Complexity {
    Simple,
    MultiStep,
}

fn parse_complexity(output: &str) -> Complexity {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("COMPLEXITY:") {
            let value = value.trim().to_lowercase();
            if value == "simple" {
                return Complexity::Simple;
            }
            if value.starts_with("multi") {
                return Complexity::MultiStep;
            }
        }
    }
    // Default to multi-step if we can't parse (safer to run the full pipeline)
    Complexity::MultiStep
}

fn parse_subtasks(output: &str) -> Vec<Subtask> {
    let mut subtasks = Vec::new();
    let mut current_desc: Option<String> = None;
    let mut current_files = String::new();

    for line in output.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("SUBTASK") {
            // Save previous subtask
            if let Some(desc) = current_desc.take() {
                subtasks.push(Subtask {
                    description: desc,
                    files: std::mem::take(&mut current_files),
                });
            }
            // Parse "SUBTASK N: description"
            if let Some(colon_pos) = rest.find(':') {
                current_desc = Some(rest[colon_pos + 1..].trim().to_string());
            }
        } else if let Some(files) = trimmed.strip_prefix("FILES:") {
            current_files = files.trim().to_string();
        }
    }

    // Don't forget the last subtask
    if let Some(desc) = current_desc.take() {
        subtasks.push(Subtask {
            description: desc,
            files: current_files,
        });
    }

    subtasks
}

enum Verdict {
    Pass(String),
    Fail(String),
}

fn parse_verdict(output: &str) -> Verdict {
    let mut verdict_is_pass: Option<bool> = None;
    let mut reason = String::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("VERDICT:") {
            let v = rest.trim().to_lowercase();
            verdict_is_pass = Some(v == "pass");
        } else if let Some(rest) = trimmed.strip_prefix("REASON:") {
            reason = rest.trim().to_string();
        }
    }

    match verdict_is_pass {
        Some(true) => Verdict::Pass(reason),
        _ => Verdict::Fail(if reason.is_empty() {
            "No clear pass verdict from judge".to_string()
        } else {
            reason
        }),
    }
}

fn get_git_diff(cwd: &str) -> String {
    let output = Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(cwd)
        .output();

    match output {
        Ok(o) => {
            let diff = String::from_utf8_lossy(&o.stdout).to_string();
            if diff.is_empty() {
                // Also check for untracked files
                let untracked = Command::new("git")
                    .args(["status", "--short"])
                    .current_dir(cwd)
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                    .unwrap_or_default();
                if untracked.is_empty() {
                    "(no changes)".to_string()
                } else {
                    format!("(no tracked diff)\nUntracked files:\n{}", untracked)
                }
            } else {
                // Truncate huge diffs
                if diff.len() > 10000 {
                    let truncated: String = diff.chars().take(10000).collect();
                    format!("{}\n... (diff truncated at 10000 chars)", truncated)
                } else {
                    diff
                }
            }
        }
        Err(_) => "(git diff failed)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_complexity_simple() {
        assert_eq!(
            parse_complexity("COMPLEXITY: simple\nJust do the thing."),
            Complexity::Simple
        );
    }

    #[test]
    fn parse_complexity_multi() {
        assert_eq!(
            parse_complexity("COMPLEXITY: multi-step\nSUBTASK 1: foo"),
            Complexity::MultiStep
        );
    }

    #[test]
    fn parse_complexity_default() {
        assert_eq!(parse_complexity("No markers here"), Complexity::MultiStep);
    }

    #[test]
    fn parse_subtasks_basic() {
        let output = r#"COMPLEXITY: multi-step

SUBTASK 1: Create the config file
FILES: src/config.rs

SUBTASK 2: Update main
FILES: src/main.rs
"#;
        let tasks = parse_subtasks(output);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].description, "Create the config file");
        assert_eq!(tasks[0].files, "src/config.rs");
        assert_eq!(tasks[1].description, "Update main");
        assert_eq!(tasks[1].files, "src/main.rs");
    }

    #[test]
    fn parse_subtasks_empty() {
        assert!(parse_subtasks("No subtasks here").is_empty());
    }

    #[test]
    fn parse_verdict_pass() {
        let output = "VERDICT: pass\nREASON: All changes look correct.";
        match parse_verdict(output) {
            Verdict::Pass(r) => assert_eq!(r, "All changes look correct."),
            Verdict::Fail(_) => panic!("Expected pass"),
        }
    }

    #[test]
    fn parse_verdict_fail() {
        let output = "VERDICT: fail\nREASON: Build errors.";
        match parse_verdict(output) {
            Verdict::Fail(r) => assert_eq!(r, "Build errors."),
            Verdict::Pass(_) => panic!("Expected fail"),
        }
    }

    #[test]
    fn parse_verdict_missing() {
        match parse_verdict("No verdict here") {
            Verdict::Fail(r) => assert!(r.contains("No clear pass")),
            Verdict::Pass(_) => panic!("Expected fail"),
        }
    }
}
