use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::{execute, terminal};

/// Handle to a running spinner task.
pub struct SpinnerHandle {
    stop: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
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

    pub fn start_spinner(&self, iteration: u32) -> SpinnerHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
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
                eprint!(
                    "\r{} Thinking... ({iteration}/{}) {elapsed}s",
                    frame, 50
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

    pub fn tool_use_start(&mut self, name: &str) {
        self.tool_input_buf.clear();
        let (icon, color) = tool_style(name);
        let _ = execute!(io::stdout(), SetForegroundColor(color));
        match name {
            "bash" => print!("\n{icon} $ "),
            "write_file" => print!("\n{icon} Writing "),
            "edit_file" => print!("\n{icon} Editing "),
            "read_file" => print!("\n{icon} Reading "),
            _ => print!("\n{icon} {name} "),
        }
        let _ = execute!(io::stdout(), ResetColor);
        let _ = io::stdout().flush();
    }

    pub fn tool_input_delta(&mut self, tool_name: &str, chunk: &str) {
        self.tool_input_buf.push_str(chunk);

        match tool_name {
            "bash" => {
                // Stream the command text
                print!("{}", chunk);
                let _ = io::stdout().flush();
            }
            "write_file" => {
                // Show path from first chunk, then stream content
                // The input is JSON being assembled, so we stream partial
                print!("{}", chunk);
                let _ = io::stdout().flush();
            }
            "read_file" | "edit_file" => {
                // For read/edit, just stream the delta
                print!("{}", chunk);
                let _ = io::stdout().flush();
            }
            _ => {}
        }
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

        // Show a compact summary of the output
        let first_line = output.lines().next().unwrap_or(output);
        if name == "bash" {
            // For bash, show first few lines of output
            let lines: Vec<&str> = output.lines().take(10).collect();
            let _ = execute!(io::stdout(), SetForegroundColor(Color::DarkGrey));
            for line in &lines {
                println!("  {line}");
            }
            let total = output.lines().count();
            if total > 10 {
                println!("  ... ({} more lines)", total - 10);
            }
            let _ = execute!(io::stdout(), ResetColor);
        } else {
            let _ = execute!(io::stdout(), SetForegroundColor(Color::DarkGrey));
            println!("{first_line}");
            let _ = execute!(io::stdout(), ResetColor);
        }
        self.tool_input_buf.clear();
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

        // Read single character using crossterm raw mode
        let result = (|| -> io::Result<bool> {
            terminal::enable_raw_mode()?;
            let answer = loop {
                if crossterm::event::poll(std::time::Duration::from_secs(30))? {
                    if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                        match key.code {
                            crossterm::event::KeyCode::Char('y' | 'Y') => break true,
                            crossterm::event::KeyCode::Char('n' | 'N')
                            | crossterm::event::KeyCode::Enter => break false,
                            _ => {}
                        }
                    }
                } else {
                    // Timeout — deny
                    break false;
                }
            };
            terminal::disable_raw_mode()?;
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
