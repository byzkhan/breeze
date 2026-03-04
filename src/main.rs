mod agent;
mod checkpoint;
mod config;
mod harness;
mod prompts;
mod provider;
mod tools;
mod ui;
mod util;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::execute;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use agent::Agent;
use config::{breeze_dir, load_config};
use harness::{Harness, HarnessResult};
use provider::anthropic::AnthropicProvider;
use tools::ToolRegistry;
use ui::Ui;

#[derive(Parser)]
#[command(name = "breeze", version, about = "CLI coding agent")]
struct Cli {
    /// Execute a single command and exit
    #[arg(short = 'x', long)]
    execute: Option<String>,

    /// Override the model
    #[arg(short = 'm', long)]
    model: Option<String>,

    /// Set working directory
    #[arg(short = 'C', long)]
    cwd: Option<String>,

    /// Enable planner→worker→judge harness pipeline
    #[arg(long)]
    harness: bool,
}

fn print_banner() {
    let _ = execute!(
        std::io::stderr(),
        SetForegroundColor(Color::Cyan)
    );
    eprintln!("breeze v{}", env!("CARGO_PKG_VERSION"));
    let _ = execute!(std::io::stderr(), ResetColor);
    eprintln!("Type /help for commands, /exit to quit.\n");
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = load_config()?;

    let model = cli
        .model
        .unwrap_or_else(|| config.default_model.clone());

    let cwd = match cli.cwd {
        Some(c) => c,
        None => {
            let dir = std::env::current_dir()
                .expect("Failed to get current working directory. Use --cwd to specify one.");
            dir.to_str()
                .expect("Current directory path contains invalid UTF-8. Use --cwd to specify one.")
                .to_string()
        }
    };

    let mut harness_enabled = cli.harness || config.harness_enabled;
    let mut ui = Ui::new();

    // Single-command mode
    if let Some(cmd) = cli.execute {
        if harness_enabled {
            let h = Harness::new(&config, &model, &cwd);
            match h.run(&cmd, &mut ui).await {
                Ok(HarnessResult::Success(msg)) => {
                    ui.print_info(&format!("[harness] Done: {}", first_line(&msg)));
                }
                Ok(HarnessResult::Passthrough(_)) => {}
                Ok(HarnessResult::Failed { reason, rolled_back }) => {
                    let rb = if rolled_back { " (rolled back)" } else { "" };
                    ui.print_error(&format!("[harness] Failed: {}{}", reason, rb));
                    std::process::exit(1);
                }
                Err(e) => {
                    ui.print_error(&e.to_string());
                    std::process::exit(1);
                }
            }
        } else {
            let provider = Box::new(AnthropicProvider::new(config.api_key.clone(), model.clone()));
            let tools = ToolRegistry::default_registry();
            let mut agent = Agent::new(cwd.clone(), provider, tools);
            match agent.run(&cmd, &mut ui).await {
                Ok(_) => {}
                Err(e) => {
                    ui.print_error(&e.to_string());
                    std::process::exit(1);
                }
            }
        }
        return Ok(());
    }

    // REPL mode — create persistent agent only when harness is off
    let mut agent: Option<Agent> = if !harness_enabled {
        let provider = Box::new(AnthropicProvider::new(config.api_key.clone(), model.clone()));
        let tools = ToolRegistry::default_registry();
        Some(Agent::new(cwd.clone(), provider, tools))
    } else {
        None
    };

    print_banner();
    if harness_enabled {
        eprintln!("Harness mode: ON (planner→worker→judge pipeline)");
    }

    let history_path = breeze_dir()
        .map(|d| d.join("history"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/.breeze_history"));

    let mut rl = DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);

    // Ctrl+C handling
    let running = Arc::new(AtomicBool::new(false));

    let running_clone = running.clone();
    ctrlc_handler(running_clone);

    loop {
        let prompt = format!(
            "{}breeze ❯ {}",
            SetForegroundColor(Color::Blue),
            ResetColor
        );
        let readline = rl.readline(&prompt);

        match readline {
            Ok(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                let _ = rl.add_history_entry(&line);

                // Slash commands
                if line.starts_with('/') {
                    match line.as_str() {
                        "/exit" | "/quit" => {
                            eprintln!("Goodbye!");
                            break;
                        }
                        "/clear" => {
                            if let Some(ref mut a) = agent {
                                a.clear();
                            }
                            eprintln!("Conversation cleared.");
                            continue;
                        }
                        "/harness" => {
                            harness_enabled = !harness_enabled;
                            if harness_enabled {
                                agent = None;
                                eprintln!("Harness mode: ON (planner→worker→judge pipeline)");
                            } else {
                                let provider = Box::new(AnthropicProvider::new(
                                    config.api_key.clone(),
                                    model.clone(),
                                ));
                                let tools = ToolRegistry::default_registry();
                                agent = Some(Agent::new(cwd.clone(), provider, tools));
                                eprintln!("Harness mode: OFF (default agent)");
                            }
                            continue;
                        }
                        "/help" => {
                            eprintln!("Commands:");
                            eprintln!("  /clear        Clear conversation history");
                            eprintln!("  /harness      Toggle harness pipeline mode");
                            eprintln!("  /model <name> Switch model");
                            eprintln!("  /exit         Exit breeze");
                            continue;
                        }
                        _ if line.starts_with("/model ") => {
                            let new_model = line.strip_prefix("/model ").unwrap().trim();
                            if new_model.is_empty() {
                                eprintln!("Usage: /model <model-name>");
                            } else {
                                eprintln!("Model switching requires restart. Use: breeze -m {new_model}");
                            }
                            continue;
                        }
                        _ => {
                            eprintln!("Unknown command: {line}. Type /help for commands.");
                            continue;
                        }
                    }
                }

                running.store(true, Ordering::SeqCst);
                if harness_enabled {
                    let h = Harness::new(&config, &model, &cwd);
                    match h.run(&line, &mut ui).await {
                        Ok(HarnessResult::Success(msg)) => {
                            ui.print_info(&format!("[harness] Done: {}", first_line(&msg)));
                        }
                        Ok(HarnessResult::Passthrough(_)) => {}
                        Ok(HarnessResult::Failed { reason, rolled_back }) => {
                            let rb = if rolled_back { " (rolled back)" } else { "" };
                            ui.print_error(&format!("[harness] Failed: {}{}", reason, rb));
                        }
                        Err(e) => {
                            ui.print_error(&e.to_string());
                        }
                    }
                } else if let Some(ref mut a) = agent {
                    match a.run(&line, &mut ui).await {
                        Ok(_) => {}
                        Err(e) => {
                            ui.print_error(&e.to_string());
                        }
                    }
                }
                running.store(false, Ordering::SeqCst);
                println!();
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl+C — if running, cancel; otherwise ignore
                if running.load(Ordering::SeqCst) {
                    eprintln!("\nCancelled.");
                    running.store(false, Ordering::SeqCst);
                }
                continue;
            }
            Err(ReadlineError::Eof) => {
                // Ctrl+D
                eprintln!("Goodbye!");
                break;
            }
            Err(e) => {
                ui.print_error(&format!("Input error: {e}"));
                break;
            }
        }
    }

    let _ = rl.save_history(&history_path);
    Ok(())
}

fn first_line(s: &str) -> String {
    s.lines()
        .next()
        .unwrap_or(s)
        .chars()
        .take(80)
        .collect()
}

/// Set up Ctrl+C handling. Rustyline handles SIGINT during readline (returns
/// ReadlineError::Interrupted). Outside readline, we rely on the default
/// signal disposition — the `running` flag lets the REPL loop distinguish
/// between idle and active states for user feedback.
fn ctrlc_handler(_running: Arc<AtomicBool>) {
    // No custom signal handler needed: rustyline already catches Ctrl+C
    // during readline and surfaces it as ReadlineError::Interrupted.
    // The REPL loop checks the `running` flag for status messages.
}
