use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

struct PtyState {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
}

#[tauri::command]
fn spawn_shell(app: AppHandle, rows: u16, cols: u16) -> Result<(), String> {
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
    let mut cmd = CommandBuilder::new(&shell);
    cmd.arg("-l");
    cmd.env("TERM", "xterm-256color");

    let _child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);

    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;

    let state = app.state::<PtyState>();
    *state.writer.lock().unwrap() = Some(writer);
    *state.master.lock().unwrap() = Some(pair.master);

    let app_handle = app.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = app_handle.emit("pty-exit", ());
                    break;
                }
                Ok(n) => {
                    let data = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = app_handle.emit("pty-data", data);
                }
                Err(_) => {
                    let _ = app_handle.emit("pty-exit", ());
                    break;
                }
            }
        }
    });

    Ok(())
}

#[tauri::command]
fn write_pty(app: AppHandle, data: String) -> Result<(), String> {
    let state = app.state::<PtyState>();
    let mut guard = state.writer.lock().unwrap();
    if let Some(ref mut writer) = *guard {
        writer
            .write_all(data.as_bytes())
            .map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn resize_pty(app: AppHandle, rows: u16, cols: u16) -> Result<(), String> {
    let state = app.state::<PtyState>();
    let guard = state.master.lock().unwrap();
    if let Some(ref master) = *guard {
        master
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .manage(PtyState {
            writer: Mutex::new(None),
            master: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![spawn_shell, write_pty, resize_pty])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
