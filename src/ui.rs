use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::{execute, terminal};

/// Handle to a running spinner task.
pub struct SpinnerHandle {
    stop: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
    message: Arc<Mutex<String>>,
}

impl SpinnerHandle {
    /// Update the spinner message while it's running.
    pub fn set_message(&self, msg: &str) {
        if let Ok(mut m) = self.message.lock() {
            *m = msg.to_string();
        }
    }
}

pub struct Ui {
    /// Whether we're currently inside a streamed text block (need trailing newline).
    in_text: bool,
    /// Current tool input accumulator for streaming display.
    tool_input_buf: String,
}

impl Ui {
    pub fn new() -> Self {
        Self {
            in_text: false,
            tool_input_buf: String::new(),
        }
    }

    // ── Spinner ────────────────────────────────────────────────

    pub fn start_spinner(&self, iteration: u32, max_iterations: u32) -> SpinnerHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let message = Arc::new(Mutex::new("Thinking...".to_string()));
        let message_clone = message.clone();
        let handle = tokio::spawn(async move {
            const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let start = Instant::now();
            let mut i = 0;
            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                let elapsed = start.elapsed().as_secs();
                let frame = FRAMES[i % FRAMES.len()];
                let msg = message_clone.lock().map(|m| m.clone()).unwrap_or_default();
                eprint!(
                    "\r{} {} ({iteration}/{max_iterations}) {elapsed}s    ",
                    frame, msg
                );
                let _ = io::stderr().flush();
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            }
            // Clear spinner line
            eprint!("\r{}\r", " ".repeat(60));
            let _ = io::stderr().flush();
        });
        SpinnerHandle {
            stop,
            handle: Some(handle),
            message,
        }
    }

    pub fn stop_spinner(&self, mut handle: SpinnerHandle) {
        handle.stop.store(true, Ordering::Relaxed);
        if let Some(h) = handle.handle.take() {
            // Don't block — the spinner will clean up on next tick
            h.abort();
        }
        // Clear any residual spinner text
        eprint!("\r{}\r", " ".repeat(60));
        let _ = io::stderr().flush();
    }

    // ── Streaming text ─────────────────────────────────────────

    pub fn print_text_delta(&mut self, text: &str) {
        self.in_text = true;
        print!("{}", text);
        let _ = io::stdout().flush();
    }

    pub fn finish_text(&mut self) {
        if self.in_text {
            println!();
            self.in_text = false;
        }
    }

    // ── Tool display ───────────────────────────────────────────

    pub fn tool_use_start(&mut self, _name: &str) {
        self.tool_input_buf.clear();
        // All tools print their summary from tool_use_complete — nothing here
    }

    pub fn tool_input_delta(&mut self, _tool_name: &str, chunk: &str) {
        // Silently accumulate for all tools — no streaming output
        self.tool_input_buf.push_str(chunk);
    }

    /// Called when a tool_use block is fully received. Prints a one-line summary.
    pub fn tool_use_complete(&mut self, name: &str, input_json: &str) {
        let parsed: serde_json::Value =
            serde_json::from_str(input_json).unwrap_or(serde_json::Value::Null);
        let (icon, color) = tool_style(name);

        let _ = execute!(io::stdout(), SetForegroundColor(color));
        match name {
            "bash" => {
                let cmd = parsed["command"].as_str().unwrap_or("...");
                println!("\n{icon} $ {cmd}");
            }
            "write_file" => {
                let path = parsed["path"].as_str().unwrap_or("unknown");
                let line_count = parsed["content"]
                    .as_str()
                    .map(|c| c.lines().count())
                    .unwrap_or(0);
                println!("\n{icon} Write {path} ({line_count} lines)");
            }
            "edit_file" => {
                let path = parsed["path"].as_str().unwrap_or("unknown");
                println!("\n{icon} Edit {path}");
            }
            "read_file" => {
                let path = parsed["path"].as_str().unwrap_or("unknown");
                println!("\n{icon} Read {path}");
            }
            _ => {
                println!("\n{icon} {name}");
            }
        }
        let _ = execute!(io::stdout(), ResetColor);
        let _ = io::stdout().flush();
    }

    pub fn tool_result(&mut self, name: &str, output: &str, success: bool) {
        println!();
        if success {
            let _ = execute!(io::stdout(), SetForegroundColor(Color::Green));
            print!("  ✓ ");
        } else {
            let _ = execute!(io::stdout(), SetForegroundColor(Color::Red));
            print!("  ✗ ");
        }
        let _ = execute!(io::stdout(), ResetColor);

        let _ = execute!(io::stdout(), SetForegroundColor(Color::DarkGrey));
        if name == "bash" {
            // For bash, show first few lines of output
            let lines: Vec<&str> = output.lines().take(10).collect();
            for line in &lines {
                println!("  {line}");
            }
            let total = output.lines().count();
            if total > 10 {
                println!("  ... ({} more lines)", total - 10);
            }
        } else {
            // For non-bash tools, show first line of output
            let first_line = output.lines().next().unwrap_or(output);
            println!("{first_line}");
        }
        let _ = execute!(io::stdout(), ResetColor);
        self.tool_input_buf.clear();
    }

    // ── Token usage ───────────────────────────────────────────

    pub fn print_usage(&self, input_tokens: u64, output_tokens: u64) {
        let _ = execute!(io::stdout(), SetForegroundColor(Color::DarkGrey));
        let fmt_in = format_tokens(input_tokens);
        let fmt_out = format_tokens(output_tokens);
        println!("  tokens: {fmt_in} in / {fmt_out} out");
        let _ = execute!(io::stdout(), ResetColor);
    }

    // ── Retry ──────────────────────────────────────────────────

    pub fn print_retry(&self, attempt: u32, max_retries: u32, delay_secs: u64, reason: &str) {
        let _ = execute!(io::stderr(), SetForegroundColor(Color::Yellow));
        eprintln!(
            "⚠ Rate limited ({}/{}) — retrying in {}s... ({})",
            attempt, max_retries, delay_secs, reason
        );
        let _ = execute!(io::stderr(), ResetColor);
    }

    // ── Messages ───────────────────────────────────────────────

    pub fn print_error(&self, msg: &str) {
        let _ = execute!(io::stderr(), SetForegroundColor(Color::Red));
        eprintln!("✗ {msg}");
        let _ = execute!(io::stderr(), ResetColor);
    }

    pub fn print_warning(&self, msg: &str) {
        let _ = execute!(io::stderr(), SetForegroundColor(Color::Yellow));
        eprintln!("⚠ {msg}");
        let _ = execute!(io::stderr(), ResetColor);
    }

    #[allow(dead_code)]
    pub fn print_info(&self, msg: &str) {
        let _ = execute!(io::stderr(), SetForegroundColor(Color::Cyan));
        eprintln!("{msg}");
        let _ = execute!(io::stderr(), ResetColor);
    }

    // ── Permission prompt ──────────────────────────────────────

    pub fn ask_permission(&self, action: &str) -> bool {
        let _ = execute!(io::stderr(), SetForegroundColor(Color::Yellow));
        eprint!("⚠ Allow: {}? [y/N] ", action);
        let _ = execute!(io::stderr(), ResetColor);
        let _ = io::stderr().flush();

        // Read single character using crossterm raw mode.
        // Always disable raw mode on exit, even if poll/read returns Err.
        let result = (|| -> io::Result<bool> {
            terminal::enable_raw_mode()?;
            let answer = loop {
                match crossterm::event::poll(std::time::Duration::from_secs(30)) {
                    Ok(true) => match crossterm::event::read() {
                        Ok(crossterm::event::Event::Key(key)) => match key.code {
                            crossterm::event::KeyCode::Char('y' | 'Y') => break true,
                            crossterm::event::KeyCode::Char('n' | 'N')
                            | crossterm::event::KeyCode::Enter => break false,
                            _ => {}
                        },
                        Ok(_) => {}
                        Err(_) => break false,
                    },
                    Ok(false) => break false, // Timeout — deny
                    Err(_) => break false,
                }
            };
            let _ = terminal::disable_raw_mode();
            Ok(answer)
        })();

        let allowed = result.unwrap_or(false);
        if allowed {
            eprintln!("y");
        } else {
            eprintln!("n");
        }
        allowed
    }
}

fn tool_style(name: &str) -> (&'static str, Color) {
    match name {
        "bash" => ("⚡", Color::Cyan),
        "write_file" => ("📝", Color::Yellow),
        "edit_file" => ("✏️ ", Color::Yellow),
        "read_file" => ("📄", Color::DarkGrey),
        _ => ("🔧", Color::White),
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}
