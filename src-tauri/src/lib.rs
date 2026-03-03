use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::HashMap;
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
    let output = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err("node not installed".to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tauri::command]
fn check_command_exists(command: String) -> bool {
    std::process::Command::new("which")
        .arg(&command)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

/// Run a shell command as a subprocess and capture output.
async fn run_shell_command(app: &AppHandle, tab_id: &str, command: &str) -> Result<String, String> {
    // Get CWD from the tab's shell process
    let cwd = {
        // Extract child_pid while holding the lock briefly
        let child_pid = {
            let state = app.state::<PtyState>();
            let sessions = state.sessions.lock().unwrap();
            sessions.get(tab_id).and_then(|s| s.child_pid)
        }; // Lock released here

        // Run lsof command outside the lock scope
        let mut found_cwd = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        if let Some(pid) = child_pid {
            if let Ok(output) = tokio::process::Command::new("lsof")
                .args(["-d", "cwd", "-a", "-p", &pid.to_string(), "-Fn"])
                .output()
                .await
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if let Some(path) = line.strip_prefix('n') {
                        found_cwd = path.to_string();
                        break;
                    }
                }
            }
        }
        found_cwd
    };

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(&shell)
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| "Command timed out after 30 seconds".to_string())?
    .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);

    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&stderr);
    }
    if output.is_empty() {
        output.push_str("(no output)");
    }

    // Truncate very long output
    const MAX_CHARS: usize = 8000;
    let char_count = output.chars().count();
    if char_count > MAX_CHARS {
        let truncate_at = output.char_indices().nth(MAX_CHARS).map(|(i, _)| i).unwrap_or(output.len());
        output.truncate(truncate_at);
        output.push_str("\n... (output truncated)");
    }

    Ok(output)
}

#[tauri::command]
async fn agent_chat(
    app: AppHandle,
    tab_id: String,
    message: String,
    history: Vec<serde_json::Value>,
) -> Result<String, String> {
    let api_key = get_api_key()?;

    let system_prompt = "You are an agentic terminal assistant embedded in a terminal emulator called Breeze. \
        You have a run_command tool to execute shell commands directly in the user's terminal.\n\n\
        Key behaviors:\n\
        - When the user asks you to do something, DO it using run_command — don't just explain how.\n\
        - Run commands proactively to gather information, fix problems, and complete tasks.\n\
        - After running a command, analyze the output and take next steps automatically.\n\
        - If a command fails, diagnose the issue and try to fix it.\n\
        - Be concise in your text responses — let the commands do the talking.\n\
        - Only ask the user for clarification when you truly cannot proceed without their input.\n\
        - Use markdown formatting for text responses (headings, code blocks, lists).";

    let tools = json!([{
        "name": "run_command",
        "description": "Run a shell command in the user's terminal and see the output. Use this whenever you need to check something, run a script, install packages, create files, or perform any terminal action. The command runs in the user's current working directory.",
        "input_schema": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            },
            "required": ["command"]
        }
    }]);

    let mut messages = history.clone();
    messages.push(json!({"role": "user", "content": message}));

    let mut full_text = String::new();
    let client = reqwest::Client::builder()
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    // Agentic loop — up to 15 tool-use iterations
    let mut exhausted_iterations = true;
    for _iteration in 0..15 {
        let res = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&json!({
                "model": "claude-sonnet-4-20250514",
                "max_tokens": 4096,
                "stream": true,
                "system": system_prompt,
                "tools": tools,
                "messages": messages
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(format!("API error {}: {}", status, body));
        }

        let mut stream = res.bytes_stream();
        let mut sse_buf = String::new();

        // Track content blocks for this turn
        let mut current_text = String::new();
        let mut current_block_type = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_json = String::new();
        let mut tool_calls: Vec<(String, String, String)> = vec![]; // (id, name, input_json)
        let mut stop_reason = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            sse_buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(line_end) = sse_buf.find('\n') {
                let line = sse_buf[..line_end].trim_end_matches('\r').to_string();
                sse_buf = sse_buf[line_end + 1..].to_string();

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
                                }
                            }
                            "content_block_delta" => {
                                if current_block_type == "text" {
                                    if let Some(text) = event["delta"]["text"].as_str() {
                                        current_text.push_str(text);
                                        full_text.push_str(text);
                                        let _ = app.emit(
                                            "agent-chunk",
                                            json!({"tab_id": tab_id, "text": text}),
                                        );
                                    }
                                } else if current_block_type == "tool_use" {
                                    if let Some(json_chunk) =
                                        event["delta"]["partial_json"].as_str()
                                    {
                                        current_tool_json.push_str(json_chunk);
                                    }
                                }
                            }
                            "content_block_stop" => {
                                if current_block_type == "tool_use" {
                                    tool_calls.push((
                                        current_tool_id.clone(),
                                        current_tool_name.clone(),
                                        current_tool_json.clone(),
                                    ));
                                }
                                current_block_type = String::new();
                            }
                            "message_delta" => {
                                if let Some(sr) = event["delta"]["stop_reason"].as_str() {
                                    stop_reason = sr.to_string();
                                }
                            }
                            "error" => {
                                let err_msg = event["error"]["message"]
                                    .as_str()
                                    .unwrap_or("Unknown stream error");
                                return Err(err_msg.to_string());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // Build the assistant message content blocks
        let mut assistant_content: Vec<serde_json::Value> = vec![];
        if !current_text.is_empty() {
            assistant_content.push(json!({"type": "text", "text": current_text}));
        }
        for (id, name, input_json) in &tool_calls {
            let input: serde_json::Value =
                serde_json::from_str(input_json).unwrap_or(json!({}));
            assistant_content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input
            }));
        }
        if assistant_content.is_empty() {
            exhausted_iterations = false;
            break;
        }
        messages.push(json!({"role": "assistant", "content": assistant_content}));

        // If stop_reason is tool_use, execute tools and loop
        if stop_reason == "tool_use" {
            if tool_calls.is_empty() {
                exhausted_iterations = false;
                break;
            }
            let mut tool_results: Vec<serde_json::Value> = vec![];

            for (id, name, input_json) in &tool_calls {
                if name == "run_command" {
                    let input: serde_json::Value =
                        serde_json::from_str(input_json).unwrap_or(json!({}));
                    let command = input["command"].as_str().unwrap_or("").to_string();

                    // Validate that command is not empty or whitespace-only
                    if command.trim().is_empty() {
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": "Error: command cannot be empty"
                        }));
                        continue;
                    }

                    // Emit event: command is about to run
                    let _ = app.emit(
                        "agent-tool-call",
                        json!({"tab_id": tab_id, "command": command}),
                    );

                    // Execute the command
                    let output = run_shell_command(&app, &tab_id, &command).await;
                    let result_text = match &output {
                        Ok(s) => s.clone(),
                        Err(e) => format!("Error: {}", e),
                    };

                    // Emit event: command finished with output
                    let _ = app.emit(
                        "agent-tool-result",
                        json!({"tab_id": tab_id, "command": command, "output": result_text}),
                    );

                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": result_text
                    }));
                } else {
                    // Unknown tool - return error result
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": format!("Error: unknown tool '{}'", name)
                    }));
                }
            }

            messages.push(json!({"role": "user", "content": tool_results}));
            continue; // Loop back to call API again
        }

        // stop_reason is "end_turn" — we're done
        exhausted_iterations = false;
        break;
    }

    // Check if the loop exhausted all iterations without reaching end_turn
    if exhausted_iterations {
        full_text.push_str("\n\n⚠️ Agent reached maximum iteration limit (15 turns). The task may be incomplete.");
    }

    Ok(full_text)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .manage(PtyState {
            sessions: Mutex::new(HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![spawn_shell, write_pty, resize_pty, close_tab, pause_pty, resume_pty, get_shell_cwd, translate_command, agent_chat, get_git_branch, get_node_version, check_command_exists])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
