use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use serde_json::json;
use futures::StreamExt;

const READ_BUF_SIZE: usize = 16 * 1024;   // 16 KB per read()
const MAX_BATCH: usize = 64 * 1024;        // flush at 64 KB

struct PtySession {
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child_pid: Option<u32>,
    paused: Arc<AtomicBool>,
}

struct PtyState {
    sessions: Mutex<HashMap<String, PtySession>>,
}

struct AgentState {
    messages: Vec<serde_json::Value>,
    todo: String,
    recent_commands: VecDeque<String>,
    recent_edits: VecDeque<String>,
    cwd: String,
    iteration: u32,
    running: bool,
}

impl AgentState {
    fn new(cwd: String) -> Self {
        Self {
            messages: Vec::new(),
            todo: String::new(),
            recent_commands: VecDeque::with_capacity(6),
            recent_edits: VecDeque::with_capacity(6),
            cwd,
            iteration: 0,
            running: false,
        }
    }
}

struct AgentStates {
    states: Mutex<HashMap<String, AgentState>>,
}

/// Returns true if `fd` has data ready to read (zero-timeout poll).
fn has_data_available(fd: i32) -> bool {
    if fd < 0 {
        return false;
    }
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // timeout = 0 → non-blocking check
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    ret > 0 && (pfd.revents & libc::POLLIN) != 0
}

fn utf8_complete_len(buf: &[u8]) -> usize {
    let mut offset = 0;
    loop {
        match std::str::from_utf8(&buf[offset..]) {
            Ok(_) => return buf.len(),
            Err(e) => {
                let valid = e.valid_up_to();
                match e.error_len() {
                    None => return offset + valid, // incomplete sequence at tail — exclude it
                    Some(bad_len) => {
                        // genuinely invalid bytes — include them, continue checking
                        offset += valid + bad_len;
                    }
                }
            }
        }
    }
}

#[tauri::command]
fn spawn_shell(app: AppHandle, tab_id: String, rows: u16, cols: u16) -> Result<(), String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    // Use `env -u` to strip Claude Code's nesting guard so `claude` works inside Breeze
    let mut cmd = CommandBuilder::new("/usr/bin/env");
    cmd.arg("-u");
    cmd.arg("CLAUDECODE");
    cmd.arg(&shell);
    cmd.arg("-l");
    cmd.env("TERM", "xterm-256color");

    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    let pid = child.process_id();
    drop(child);
    drop(pair.slave);

    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;

    // Get raw fd for poll() — used to check if more data is immediately available
    let reader_fd = pair.master.as_raw_fd().unwrap_or(-1);

    let paused = Arc::new(AtomicBool::new(false));

    let state = app.state::<PtyState>();
    {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.insert(tab_id.clone(), PtySession {
            writer,
            master: pair.master,
            child_pid: pid,
            paused: Arc::clone(&paused),
        });
    }

    let app_handle = app.clone();
    let tid = tab_id.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; READ_BUF_SIZE];
        let mut accum: Vec<u8> = Vec::new();
        let fd = reader_fd;

        /// Flush accumulated data as a UTF-8 safe event, returning any
        /// incomplete trailing bytes back into `accum`.
        fn flush_accum(accum: &mut Vec<u8>, app: &AppHandle, tid: &str) {
            if accum.is_empty() {
                return;
            }
            let valid_up_to = utf8_complete_len(accum);
            if valid_up_to > 0 {
                let data = String::from_utf8_lossy(&accum[..valid_up_to]).into_owned();
                let _ = app.emit("pty-data", json!({ "tab_id": tid, "data": data }));
            }
            // Keep any incomplete trailing UTF-8 bytes
            let leftover = accum[valid_up_to..].to_vec();
            *accum = leftover;
        }

        loop {
            // Flow-control: spin-wait while paused (check every 5ms)
            while paused.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            match reader.read(&mut buf) {
                Ok(0) => {
                    flush_accum(&mut accum, &app_handle, &tid);
                    let _ = app_handle.emit("pty-exit", json!({ "tab_id": tid }));
                    if let Some(state) = app_handle.try_state::<PtyState>() {
                        let mut sessions = state.sessions.lock().unwrap();
                        sessions.remove(&tid);
                    }
                    break;
                }
                Ok(n) => {
                    accum.extend_from_slice(&buf[..n]);

                    // Keep reading while more data is immediately available
                    // and we haven't hit the batch cap
                    while accum.len() < MAX_BATCH && has_data_available(fd) {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n2) => accum.extend_from_slice(&buf[..n2]),
                            Err(_) => break,
                        }
                    }

                    flush_accum(&mut accum, &app_handle, &tid);
                }
                Err(_) => {
                    flush_accum(&mut accum, &app_handle, &tid);
                    let _ = app_handle.emit("pty-exit", json!({ "tab_id": tid }));
                    if let Some(state) = app_handle.try_state::<PtyState>() {
                        let mut sessions = state.sessions.lock().unwrap();
                        sessions.remove(&tid);
                    }
                    break;
                }
            }
        }
    });

    Ok(())
}

#[tauri::command]
fn write_pty(app: AppHandle, tab_id: String, data: String) -> Result<(), String> {
    let state = app.state::<PtyState>();
    let mut sessions = state.sessions.lock().unwrap();
    let session = sessions.get_mut(&tab_id).ok_or_else(|| format!("Session not found: {}", tab_id))?;
    session.writer.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    session.writer.flush().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn resize_pty(app: AppHandle, tab_id: String, rows: u16, cols: u16) -> Result<(), String> {
    let state = app.state::<PtyState>();
    let sessions = state.sessions.lock().unwrap();
    if let Some(session) = sessions.get(&tab_id) {
        session.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn close_tab(app: AppHandle, tab_id: String) -> Result<(), String> {
    let state = app.state::<PtyState>();
    let mut sessions = state.sessions.lock().unwrap();
    sessions.remove(&tab_id);
    Ok(())
}

#[tauri::command]
fn get_shell_cwd(app: AppHandle, tab_id: String) -> Result<String, String> {
    let state = app.state::<PtyState>();
    let sessions = state.sessions.lock().unwrap();
    let session = sessions.get(&tab_id).ok_or("Tab not found")?;
    let pid = session.child_pid.ok_or("Shell not started")?;

    #[cfg(not(unix))]
    return Err("CWD detection not supported on this platform".to_string());

    #[cfg(unix)]
    let output = std::process::Command::new("lsof")
        .args(["-d", "cwd", "-a", "-p", &pid.to_string(), "-Fn"])
        .output()
        .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix('n') {
            return Ok(path.to_string());
        }
    }

    Err("Could not determine shell CWD".to_string())
}

#[tauri::command]
fn get_git_branch(cwd: String) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&cwd)
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err("Not a git repo".to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tauri::command]
fn get_node_version() -> Result<String, String> {
    // Set current_dir to a known-safe directory to prevent CWD-based binary injection on Windows
    let safe_dir = std::env::temp_dir();
    let output = std::process::Command::new("node")
        .arg("--version")
        .current_dir(safe_dir)
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err("node not installed".to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tauri::command]
fn check_command_exists(command: String) -> bool {
    #[cfg(unix)]
    let result = std::process::Command::new("which")
        .arg(&command)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    #[cfg(windows)]
    let result = std::process::Command::new("where")
        .arg(&command)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    result
}

#[tauri::command]
fn pause_pty(app: AppHandle, tab_id: String) -> Result<(), String> {
    let state = app.state::<PtyState>();
    let sessions = state.sessions.lock().unwrap();
    if let Some(session) = sessions.get(&tab_id) {
        session.paused.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
fn resume_pty(app: AppHandle, tab_id: String) -> Result<(), String> {
    let state = app.state::<PtyState>();
    let sessions = state.sessions.lock().unwrap();
    if let Some(session) = sessions.get(&tab_id) {
        session.paused.store(false, Ordering::Relaxed);
    }
    Ok(())
}

fn get_api_key() -> Result<String, String> {
    // 1. Compile-time embedded key
    if let Some(key) = option_env!("BREEZE_API_KEY") {
        let key = key.trim();
        if !key.is_empty() {
            return Ok(key.to_string());
        }
    }
    // 2. Fallback: ~/.breeze/api_key file
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "HOME or USERPROFILE not set".to_string())?;
    let path = std::path::PathBuf::from(home).join(".breeze").join("api_key");
    let key = std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .map_err(|_| "No API key configured. Place your key in ~/.breeze/api_key".to_string())?;
    if key.is_empty() {
        return Err("No API key configured. Place your key in ~/.breeze/api_key".to_string());
    }
    Ok(key)
}

#[tauri::command]
async fn translate_command(prompt: String, cwd: String, history: Vec<String>) -> Result<String, String> {
    let api_key = get_api_key()?;

    let mut system_prompt = String::from(
        "You translate plain English into terminal commands. Return ONLY valid JSON with no markdown fences.\n\n\
         Format: {\"command\": \"<shell command>\", \"explanation\": \"<brief explanation>\", \"dangerous\": <true|false>}\n\n\
         Set dangerous=true for commands that delete files, modify system config, use sudo, or are otherwise destructive."
    );

    system_prompt.push_str(&format!("\n\nCurrent working directory: {}", cwd));

    if !history.is_empty() {
        system_prompt.push_str("\n\nRecent commands:");
        for cmd in &history {
            system_prompt.push_str(&format!("\n- {}", cmd));
        }
    }

    let client = reqwest::Client::builder()
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 512,
            "system": system_prompt,
            "messages": [{"role": "user", "content": prompt}]
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        return Err(format!("API error {}: {}", status, body));
    }

    let body: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    body["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "Unexpected API response format".to_string())
}

/// Resolve the CWD from the tab's shell process via lsof (Unix) or fallback to HOME.
async fn resolve_cwd(app: &AppHandle, tab_id: &str) -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/".to_string());

    #[cfg(unix)]
    {
        let child_pid = {
            let state = app.state::<PtyState>();
            let sessions = state.sessions.lock().unwrap();
            sessions.get(tab_id).and_then(|s| s.child_pid)
        };

        if let Some(pid) = child_pid {
            if let Ok(output) = tokio::process::Command::new("lsof")
                .args(["-d", "cwd", "-a", "-p", &pid.to_string(), "-Fn"])
                .output()
                .await
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if let Some(path) = line.strip_prefix('n') {
                        return path.to_string();
                    }
                }
            }
        }
    }

    #[cfg(not(unix))]
    let _ = (app, tab_id); // suppress unused warnings on non-Unix

    home
}

/// Resolve a path against the agent CWD (handles relative + absolute).
fn resolve_path(cwd: &str, path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::path::Path::new(cwd).join(p)
    }
}

/// Truncate output to a character limit.
fn truncate_output(output: &mut String, max_chars: usize) {
    let char_count = output.chars().count();
    if char_count > max_chars {
        let truncate_at = output.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(output.len());
        output.truncate(truncate_at);
        output.push_str(&format!("\n... (output truncated, showed {max_chars} of {char_count} chars)"));
    }
}

/// Execute a bash command with configurable timeout.
async fn execute_bash(app: &AppHandle, tab_id: &str, cwd: &str, command: &str, timeout_secs: u64) -> (String, bool) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let timeout = timeout_secs.min(300).max(1);

    let _ = app.emit("agent-tool-start", json!({
        "tab_id": tab_id,
        "tool": "bash",
        "detail": command,
    }));

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
        Err(e) => {
            let output = format!("Failed to execute: {e}");
            let _ = app.emit("agent-tool-end", json!({
                "tab_id": tab_id, "tool": "bash", "detail": command,
                "output": output, "success": false,
            }));
            return (output, false);
        }
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout),
        child.wait_with_output(),
    )
    .await;

    let (mut output, success) = match result {
        Err(_) => {
            // Timeout — kill_on_drop ensures cleanup when child is dropped here
            (format!("Command timed out after {timeout}s (use & for long-running processes)"), false)
        }
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
                if !out.is_empty() { out.push('\n'); }
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

    let _ = app.emit("agent-tool-end", json!({
        "tab_id": tab_id,
        "tool": "bash",
        "detail": command,
        "output": output,
        "success": success,
    }));

    (output, success)
}

/// Read a file with line numbers, optional offset/limit.
fn execute_read_file(cwd: &str, path: &str, offset: Option<u32>, limit: Option<u32>) -> (String, bool) {
    let resolved = resolve_path(cwd, path);
    let content = match std::fs::read_to_string(&resolved) {
        Ok(c) => c,
        Err(e) => return (format!("Error reading {}: {e}", resolved.display()), false),
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = offset.unwrap_or(0) as usize;
    let count = limit.unwrap_or(500) as usize;
    let end = (start + count).min(total);

    if start >= total {
        return (format!("{} has {total} lines, offset {start} is past end", resolved.display()), false);
    }

    let mut output = format!("# {path} ({total} lines)\n");
    for (i, line) in lines[start..end].iter().enumerate() {
        output.push_str(&format!("{:>4} | {}\n", start + i + 1, line));
    }
    if end < total {
        output.push_str(&format!("... ({} more lines, use offset={end} to continue)\n", total - end));
    }

    (output, true)
}

/// Create or overwrite a file.
fn execute_write_file(cwd: &str, path: &str, content: &str) -> (String, bool) {
    let resolved = resolve_path(cwd, path);

    // Auto-create parent dirs
    if let Some(parent) = resolved.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return (format!("Error creating directory {}: {e}", parent.display()), false);
            }
        }
    }

    match std::fs::write(&resolved, content) {
        Ok(_) => {
            let line_count = content.lines().count();
            (format!("Wrote {} ({line_count} lines)", resolved.display()), true)
        }
        Err(e) => (format!("Error writing {}: {e}", resolved.display()), false),
    }
}

/// Find-and-replace edit with uniqueness check.
fn execute_edit_file(cwd: &str, path: &str, old_string: &str, new_string: &str) -> (String, bool) {
    let resolved = resolve_path(cwd, path);
    let content = match std::fs::read_to_string(&resolved) {
        Ok(c) => c,
        Err(e) => return (format!("Error reading {}: {e}", resolved.display()), false),
    };

    let count = content.matches(old_string).count();
    if count == 0 {
        return (format!("Error: old_string not found in {}. Use read_file to see current content.", resolved.display()), false);
    }
    if count > 1 {
        return (format!("Error: old_string appears {count} times in {}. Include more surrounding context to make it unique.", resolved.display()), false);
    }

    let new_content = content.replacen(old_string, new_string, 1);
    match std::fs::write(&resolved, &new_content) {
        Ok(_) => (format!("Applied edit to {}", resolved.display()), true),
        Err(e) => (format!("Error writing {}: {e}", resolved.display()), false),
    }
}

/// Dispatch tool execution.
async fn execute_tool(app: &AppHandle, tab_id: &str, tool_name: &str, input: &serde_json::Value) -> (String, bool) {
    // Get CWD from agent state
    let cwd = {
        let state = app.state::<AgentStates>();
        let states = state.states.lock().unwrap();
        states.get(tab_id).map(|s| s.cwd.clone()).unwrap_or_else(|| "/".to_string())
    };

    match tool_name {
        "bash" => {
            let command = input["command"].as_str().unwrap_or("").to_string();
            if command.trim().is_empty() {
                return ("Error: command cannot be empty".to_string(), false);
            }
            let timeout = input["timeout"].as_u64().unwrap_or(120);
            execute_bash(app, tab_id, &cwd, &command, timeout).await
        }
        "read_file" => {
            let path = input["path"].as_str().unwrap_or("");
            if path.is_empty() {
                return ("Error: path is required".to_string(), false);
            }
            let offset = input["offset"].as_u64().map(|v| v as u32);
            let limit = input["limit"].as_u64().map(|v| v as u32);

            let _ = app.emit("agent-tool-start", json!({
                "tab_id": tab_id, "tool": "read_file", "detail": path,
            }));
            let (output, success) = execute_read_file(&cwd, path, offset, limit);
            let _ = app.emit("agent-tool-end", json!({
                "tab_id": tab_id, "tool": "read_file", "detail": path,
                "output": if output.chars().count() > 500 {
                    let trunc_at = output.char_indices().nth(500).map(|(i, _)| i).unwrap_or(output.len());
                    format!("{}... ({} chars)", &output[..trunc_at], output.len())
                } else { output.clone() },
                "success": success,
            }));
            (output, success)
        }
        "write_file" => {
            let path = input["path"].as_str().unwrap_or("");
            if path.is_empty() {
                return ("Error: path is required".to_string(), false);
            }
            let content = input["content"].as_str().unwrap_or("");

            let _ = app.emit("agent-tool-start", json!({
                "tab_id": tab_id, "tool": "write_file", "detail": path,
            }));
            let (output, success) = execute_write_file(&cwd, path, content);
            let _ = app.emit("agent-tool-end", json!({
                "tab_id": tab_id, "tool": "write_file", "detail": path,
                "output": output, "success": success,
            }));
            (output, success)
        }
        "edit_file" => {
            let path = input["path"].as_str().unwrap_or("");
            if path.is_empty() {
                return ("Error: path is required".to_string(), false);
            }
            let old_string = input["old_string"].as_str().unwrap_or("");
            let new_string = input["new_string"].as_str().unwrap_or("");
            if old_string.is_empty() {
                return ("Error: old_string is required".to_string(), false);
            }

            let _ = app.emit("agent-tool-start", json!({
                "tab_id": tab_id, "tool": "edit_file", "detail": path,
            }));
            let (output, success) = execute_edit_file(&cwd, path, old_string, new_string);
            let _ = app.emit("agent-tool-end", json!({
                "tab_id": tab_id, "tool": "edit_file", "detail": path,
                "output": output, "success": success,
            }));
            (output, success)
        }
        "todo" => {
            let plan = input["plan"].as_str().unwrap_or("").to_string();
            // Update todo in agent state
            {
                let agent_states = app.state::<AgentStates>();
                let mut states = agent_states.states.lock().unwrap();
                if let Some(agent_state) = states.get_mut(tab_id) {
                    agent_state.todo = plan.clone();
                }
            }
            let _ = app.emit("agent-todo", json!({
                "tab_id": tab_id, "plan": plan,
            }));
            ("Plan updated.".to_string(), true)
        }
        _ => (format!("Error: unknown tool '{tool_name}'"), false),
    }
}

/// Compress older tool_result observations to save context.
/// Keeps last 5 tool results at full detail, compresses older ones.
fn compress_observations(messages: &mut Vec<serde_json::Value>) {
    // Count tool_result blocks from newest to oldest
    let mut tool_result_indices: Vec<(usize, usize)> = vec![]; // (msg_idx, block_idx)
    for (msg_idx, msg) in messages.iter().enumerate() {
        if let Some(content) = msg["content"].as_array() {
            for (block_idx, block) in content.iter().enumerate() {
                if block["type"].as_str() == Some("tool_result") {
                    tool_result_indices.push((msg_idx, block_idx));
                }
            }
        }
    }

    if tool_result_indices.len() <= 5 {
        return;
    }

    // Compress all but the last 5
    let to_compress = tool_result_indices.len() - 5;
    for &(msg_idx, block_idx) in &tool_result_indices[..to_compress] {
        if let Some(content_arr) = messages[msg_idx]["content"].as_array_mut() {
            if let Some(block) = content_arr.get_mut(block_idx) {
                if let Some(text) = block["content"].as_str() {
                    let lines: Vec<&str> = text.lines().collect();
                    if lines.len() > 40 {
                        let mut compressed = String::new();
                        for line in &lines[..20] {
                            compressed.push_str(line);
                            compressed.push('\n');
                        }
                        compressed.push_str(&format!("... ({} lines omitted)\n", lines.len() - 40));
                        for line in &lines[lines.len()-20..] {
                            compressed.push_str(line);
                            compressed.push('\n');
                        }
                        block["content"] = serde_json::Value::String(compressed);
                    }
                }
            }
        }
    }

    // Also remove stale system reminder text blocks from older user messages (keep last 3)
    let mut reminder_positions: Vec<(usize, usize)> = vec![];
    for (msg_idx, msg) in messages.iter().enumerate() {
        if msg["role"].as_str() != Some("user") { continue; }
        if let Some(content) = msg["content"].as_array() {
            for (block_idx, block) in content.iter().enumerate() {
                if block["type"].as_str() == Some("text") {
                    if let Some(t) = block["text"].as_str() {
                        if t.starts_with("[Reminder:") {
                            reminder_positions.push((msg_idx, block_idx));
                        }
                    }
                }
            }
        }
    }
    if reminder_positions.len() > 3 {
        let to_remove = reminder_positions.len() - 3;
        // Remove from highest index first to avoid shifting
        for &(msg_idx, block_idx) in reminder_positions[..to_remove].iter().rev() {
            if let Some(content_arr) = messages[msg_idx]["content"].as_array_mut() {
                if content_arr.len() > 1 {
                    content_arr.remove(block_idx);
                }
            }
        }
    }
}

/// Build a system reminder to inject after tool results.
fn build_system_reminder(todo: &str, iterations_remaining: u32) -> String {
    let mut reminder = String::from("[Reminder: Read files before editing. Use edit_file for modifications.");

    if iterations_remaining <= 5 {
        reminder.push_str(&format!(" | WARNING: Only {} iterations remaining. Wrap up.", iterations_remaining));
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
fn detect_loop(recent_commands: &VecDeque<String>, recent_edits: &VecDeque<String>, tool_type: &str, identifier: &str) -> Option<String> {
    match tool_type {
        "bash" => {
            let count = recent_commands.iter().filter(|c| *c == identifier).count();
            if count >= 3 {
                Some("Warning: You've run this exact command 3+ times. Try a different approach.".to_string())
            } else {
                None
            }
        }
        "edit_file" => {
            let count = recent_edits.iter().filter(|f| *f == identifier).count();
            if count >= 3 {
                Some("Warning: You've edited this file 3+ times. Read the full file first and reconsider your approach.".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

const AGENT_SYSTEM_PROMPT: &str = r#"You are Breeze, a terminal-embedded coding agent with direct access to the user's filesystem and shell.

## Tools (5 primitives)
- bash: shell commands (git, build, test, search via grep/find/rg). Commands time out after 120s. Use & for servers.
- read_file: read with line numbers. Use offset/limit for >500 lines. ALWAYS read before editing.
- write_file: create new files or full rewrites. Auto-creates parent dirs.
- edit_file: find-replace (old_string must appear exactly once, include enough context). More efficient than rewriting.
- todo: track your plan in checklist format. Use for multi-step tasks.

## Workflow: Understand → Plan → Implement → Verify
1. Read files, check project structure with bash
2. For complex tasks (3+ steps), use todo to write a checklist
3. Make changes with edit_file (preferred) or write_file (new files)
4. Run tests, linters, build commands to verify

## Rules
- ALWAYS read a file before editing it
- If something fails, diagnose root cause — don't blindly retry
- Be concise. Let actions speak.
- Don't ask permission — just do the work
- Don't delete files without user requesting it
- Do NOT run interactive commands (vim, nano, less, top). Use non-interactive alternatives."#;

fn build_tools() -> serde_json::Value {
    json!([
        {
            "name": "bash",
            "description": "Run a shell command. Use for git, build, test, search (grep/find/rg), install, and any terminal action.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to execute" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (default 120, max 300)" }
                },
                "required": ["command"]
            }
        },
        {
            "name": "read_file",
            "description": "Read a file with line numbers. ALWAYS read before editing. Use offset/limit for large files.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative to CWD or absolute)" },
                    "offset": { "type": "integer", "description": "Starting line (0-indexed, default 0)" },
                    "limit": { "type": "integer", "description": "Max lines to read (default 500)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "write_file",
            "description": "Create a new file or completely overwrite an existing one. Auto-creates parent directories.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative to CWD or absolute)" },
                    "content": { "type": "string", "description": "Full file content to write" }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "edit_file",
            "description": "Find and replace text in a file. old_string must appear exactly once — include enough context lines to be unique.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative to CWD or absolute)" },
                    "old_string": { "type": "string", "description": "Exact text to find (must appear exactly once)" },
                    "new_string": { "type": "string", "description": "Replacement text" }
                },
                "required": ["path", "old_string", "new_string"]
            }
        },
        {
            "name": "todo",
            "description": "Track your plan as a checklist. Use this for multi-step tasks to organize your work.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "plan": { "type": "string", "description": "Your plan in checklist format (e.g. '- [x] Step 1\\n- [ ] Step 2')" }
                },
                "required": ["plan"]
            }
        }
    ])
}

/// Parse SSE stream and extract text, tool_calls, and stop_reason.
async fn process_stream(
    app: &AppHandle,
    tab_id: &str,
    response: reqwest::Response,
) -> Result<(String, Vec<(String, String, String)>, String), String> {
    let mut stream = response.bytes_stream();
    let mut sse_buf = String::new();
    let mut current_text = String::new();
    let mut current_block_type = String::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();
    let mut current_tool_json = String::new();
    let mut tool_calls: Vec<(String, String, String)> = vec![];
    let mut stop_reason = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        sse_buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(line_end) = sse_buf.find('\n') {
            let line = sse_buf[..line_end].trim_end_matches('\r').to_string();
            sse_buf = sse_buf[line_end + 1..].to_string();

            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" { continue; }
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    let event_type = event["type"].as_str().unwrap_or("");
                    match event_type {
                        "content_block_start" => {
                            let block_type = event["content_block"]["type"].as_str().unwrap_or("");
                            current_block_type = block_type.to_string();
                            if block_type == "tool_use" {
                                current_tool_id = event["content_block"]["id"].as_str().unwrap_or("").to_string();
                                current_tool_name = event["content_block"]["name"].as_str().unwrap_or("").to_string();
                                current_tool_json = String::new();
                            }
                        }
                        "content_block_delta" => {
                            if current_block_type == "text" {
                                if let Some(text) = event["delta"]["text"].as_str() {
                                    current_text.push_str(text);
                                    let _ = app.emit("agent-chunk", json!({"tab_id": tab_id, "text": text}));
                                }
                            } else if current_block_type == "tool_use" {
                                if let Some(json_chunk) = event["delta"]["partial_json"].as_str() {
                                    current_tool_json.push_str(json_chunk);
                                }
                            }
                        }
                        "content_block_stop" => {
                            if current_block_type == "tool_use" {
                                tool_calls.push((current_tool_id.clone(), current_tool_name.clone(), current_tool_json.clone()));
                            }
                            current_block_type = String::new();
                        }
                        "message_delta" => {
                            if let Some(sr) = event["delta"]["stop_reason"].as_str() {
                                stop_reason = sr.to_string();
                            }
                        }
                        "error" => {
                            let err_msg = event["error"]["message"].as_str().unwrap_or("Unknown stream error");
                            return Err(err_msg.to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Post-loop finalization: parse any trailing data left in the SSE buffer
    if !sse_buf.is_empty() {
        let line = sse_buf.trim_end_matches('\r').trim_end_matches('\n');
        if let Some(data) = line.strip_prefix("data: ") {
            if data != "[DONE]" {
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    match event["type"].as_str() {
                        Some("message_delta") => {
                            if let Some(sr) = event["delta"]["stop_reason"].as_str() {
                                stop_reason = sr.to_string();
                            }
                        }
                        Some("error") => {
                            let err_msg = event["error"]["message"].as_str().unwrap_or("Unknown stream error");
                            return Err(err_msg.to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // If an in-progress tool_use block was never closed, capture it
    if current_block_type == "tool_use" && !current_tool_id.is_empty() {
        tool_calls.push((current_tool_id, current_tool_name, current_tool_json));
    }

    Ok((current_text, tool_calls, stop_reason))
}

#[tauri::command]
async fn agent_chat(
    app: AppHandle,
    tab_id: String,
    message: String,
) -> Result<String, String> {
    let api_key = get_api_key()?;
    let max_iterations: u32 = 50;

    // Initialize or retrieve agent state
    let cwd = resolve_cwd(&app, &tab_id).await;
    {
        let agent_states = app.state::<AgentStates>();
        let mut states = agent_states.states.lock().unwrap();
        let state = states.entry(tab_id.clone()).or_insert_with(|| AgentState::new(cwd.clone()));
        if state.running {
            return Err("Agent is already running for this tab".to_string());
        }
        state.running = true;
        state.cwd = cwd.clone();
        state.iteration = 0;
        state.messages.push(json!({"role": "user", "content": message}));
    }

    let system_prompt = format!("{}\n\nCurrent directory: {}", AGENT_SYSTEM_PROMPT, cwd);
    let tools = build_tools();

    let client = reqwest::Client::builder()
        .read_timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| { mark_not_running(&app, &tab_id); e.to_string() })?;

    let mut full_text = String::new();
    let mut iteration: u32 = 0;
    let mut finished = false;

    loop {
        if iteration >= max_iterations {
            let warn = format!("\n\nReached maximum iteration limit ({max_iterations} turns). The task may be incomplete.");
            full_text.push_str(&warn);
            let _ = app.emit("agent-chunk", json!({"tab_id": tab_id, "text": warn}));
            break;
        }

        iteration += 1;

        // Emit thinking event
        let _ = app.emit("agent-thinking", json!({"tab_id": tab_id, "iteration": iteration, "total": max_iterations}));

        // Compress observations before API call
        let messages_snapshot = {
            let agent_states = app.state::<AgentStates>();
            let mut states = agent_states.states.lock().unwrap();
            if let Some(state) = states.get_mut(&tab_id) {
                state.iteration = iteration;
                compress_observations(&mut state.messages);
                state.messages.clone()
            } else {
                break;
            }
        };

        let res = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&json!({
                "model": "claude-opus-4-6",
                "max_tokens": 16384,
                "stream": true,
                "system": system_prompt,
                "tools": tools,
                "messages": messages_snapshot
            }))
            .send()
            .await;

        let res = match res {
            Ok(r) => r,
            Err(e) => {
                mark_not_running(&app, &tab_id);
                return Err(e.to_string());
            }
        };

        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            mark_not_running(&app, &tab_id);
            return Err(format!("API error {}: {}", status, body));
        }

        let (current_text, tool_calls, stop_reason) = match process_stream(&app, &tab_id, res).await {
            Ok(result) => result,
            Err(e) => {
                mark_not_running(&app, &tab_id);
                return Err(e);
            }
        };

        full_text.push_str(&current_text);

        // Build assistant content blocks and push to state
        let mut assistant_content: Vec<serde_json::Value> = vec![];
        if !current_text.is_empty() {
            assistant_content.push(json!({"type": "text", "text": current_text}));
        }
        for (id, name, input_json) in &tool_calls {
            let input: serde_json::Value = serde_json::from_str(input_json).unwrap_or(json!({}));
            assistant_content.push(json!({
                "type": "tool_use", "id": id, "name": name, "input": input
            }));
        }

        if assistant_content.is_empty() {
            finished = true;
        }

        {
            let agent_states = app.state::<AgentStates>();
            let mut states = agent_states.states.lock().unwrap();
            if let Some(state) = states.get_mut(&tab_id) {
                if !assistant_content.is_empty() {
                    state.messages.push(json!({"role": "assistant", "content": assistant_content}));
                }
            }
        }

        if finished { break; }

        // Handle tool calls
        if stop_reason == "tool_use" && !tool_calls.is_empty() {
            let mut tool_results: Vec<serde_json::Value> = vec![];

            for (id, name, input_json) in &tool_calls {
                let input: serde_json::Value = serde_json::from_str(input_json).unwrap_or(json!({}));

                let (mut output, success) = execute_tool(&app, &tab_id, name, &input).await;

                // Loop detection + tracking
                {
                    let agent_states = app.state::<AgentStates>();
                    let mut states = agent_states.states.lock().unwrap();
                    if let Some(state) = states.get_mut(&tab_id) {
                        if name == "bash" {
                            let cmd = input["command"].as_str().unwrap_or("").to_string();
                            if let Some(warning) = detect_loop(&state.recent_commands, &state.recent_edits, "bash", &cmd) {
                                output.push('\n');
                                output.push_str(&warning);
                            }
                            state.recent_commands.push_back(cmd);
                            if state.recent_commands.len() > 5 { state.recent_commands.pop_front(); }
                        } else if name == "edit_file" {
                            let path = input["path"].as_str().unwrap_or("").to_string();
                            if let Some(warning) = detect_loop(&state.recent_commands, &state.recent_edits, "edit_file", &path) {
                                output.push('\n');
                                output.push_str(&warning);
                            }
                            state.recent_edits.push_back(path);
                            if state.recent_edits.len() > 5 { state.recent_edits.pop_front(); }
                        }
                    }
                }

                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": output,
                    "is_error": !success
                }));
            }

            // Build system reminder
            let reminder = {
                let agent_states = app.state::<AgentStates>();
                let states = agent_states.states.lock().unwrap();
                let todo = states.get(&tab_id).map(|s| s.todo.as_str()).unwrap_or("");
                let remaining = max_iterations.saturating_sub(iteration);
                build_system_reminder(todo, remaining)
            };

            // Inject reminder as text block alongside tool results
            tool_results.push(json!({
                "type": "text",
                "text": reminder
            }));

            // Push tool results to agent state
            {
                let agent_states = app.state::<AgentStates>();
                let mut states = agent_states.states.lock().unwrap();
                if let Some(state) = states.get_mut(&tab_id) {
                    state.messages.push(json!({"role": "user", "content": tool_results}));
                }
            }

            continue;
        }

        // end_turn or no tool calls — done
        break;
    }

    mark_not_running(&app, &tab_id);
    Ok(full_text)
}

/// Helper to mark agent as not running.
fn mark_not_running(app: &AppHandle, tab_id: &str) {
    let agent_states = app.state::<AgentStates>();
    let mut states = agent_states.states.lock().unwrap();
    if let Some(state) = states.get_mut(tab_id) {
        state.running = false;
    }
}

#[tauri::command]
fn reset_agent(app: AppHandle, tab_id: String) -> Result<(), String> {
    let state = app.state::<AgentStates>();
    let mut states = state.states.lock().unwrap();
    states.remove(&tab_id);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .manage(PtyState {
            sessions: Mutex::new(HashMap::new()),
        })
        .manage(AgentStates {
            states: Mutex::new(HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![spawn_shell, write_pty, resize_pty, close_tab, pause_pty, resume_pty, get_shell_cwd, translate_command, agent_chat, reset_agent, get_git_branch, get_node_version, check_command_exists])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
