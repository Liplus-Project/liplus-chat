use parking_lot::Mutex;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde_json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

use crate::stream_parser::{self, CliKind};

struct PtyInstance {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send>,
    killer: Box<dyn portable_pty::MasterPty + Send>,
}

/// Pipe-based process instance (retained for write_pty compatibility).
#[allow(dead_code)]
struct PipeInstance {
    stdin: std::process::ChildStdin,
    child: std::process::Child,
}

#[allow(dead_code)]
enum ProcessInstance {
    Pty(PtyInstance),
    Pipe(PipeInstance),
}

type ProcessMap = Arc<Mutex<HashMap<String, ProcessInstance>>>;

#[derive(Clone)]
pub struct PtyState {
    procs: ProcessMap,
}

impl PtyState {
    pub fn new() -> Self {
        PtyState {
            procs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[tauri::command]
pub fn spawn_pty(
    app: AppHandle,
    state: tauri::State<PtyState>,
    command: String,
    args: Vec<String>,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
) -> Result<String, String> {
    let pty_system = NativePtySystem::default();

    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = pty_system
        .openpty(size)
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    // On Windows, .cmd/.bat scripts (like npm-installed CLIs) cannot be spawned directly.
    // Wrap them with cmd.exe /C so Windows resolves the command via PATH and PATHEXT.
    let mut cmd = if cfg!(windows) {
        let mut c = CommandBuilder::new("cmd.exe");
        c.arg("/C");
        c.arg(&command);
        for arg in &args {
            c.arg(arg);
        }
        c
    } else {
        let mut c = CommandBuilder::new(&command);
        for arg in &args {
            c.arg(arg);
        }
        c
    };

    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }

    // Spawn the child process in the PTY
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn '{command}': {e}"))?;

    // Get writer (stdin to the PTY master)
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to get PTY writer: {e}"))?;

    // Get reader (stdout from the PTY master) — clone master for resize later
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to get PTY reader: {e}"))?;

    let id = Uuid::new_v4().to_string();
    let id_clone = id.clone();
    let app_clone = app.clone();

    // Spawn background thread to read PTY output and emit events
    let ptys_clone = Arc::clone(&state.procs);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = app_clone.emit(&format!("pty-data-{}", id_clone), data);
                }
                Err(_) => break,
            }
        }
        // Collect exit code before emitting exit event
        let exit_code: Option<u32> = {
            let mut map = ptys_clone.lock();
            if let Some(ProcessInstance::Pty(pty)) = map.get_mut(&id_clone) {
                pty.child.wait().ok().map(|status| status.exit_code())
            } else {
                None
            }
        };
        // Emit exit event with exit code payload (None if killed by signal/unknown)
        let _ = app_clone.emit(&format!("pty-exit-{}", id_clone), exit_code);
    });

    state.procs.lock().insert(
        id.clone(),
        ProcessInstance::Pty(PtyInstance {
            writer,
            child,
            killer: pair.master,
        }),
    );

    Ok(id)
}

#[tauri::command]
pub fn write_pty(state: tauri::State<PtyState>, id: String, data: String) -> Result<(), String> {
    let mut map = state.procs.lock();
    let proc = map
        .get_mut(&id)
        .ok_or_else(|| format!("Process '{id}' not found"))?;
    match proc {
        ProcessInstance::Pty(pty) => pty.writer.write_all(data.as_bytes()),
        ProcessInstance::Pipe(pipe) => pipe.stdin.write_all(data.as_bytes()),
    }
    .map_err(|e| format!("Write failed: {e}"))
}

#[tauri::command]
pub fn resize_pty(
    state: tauri::State<PtyState>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let map = state.procs.lock();
    let proc = map
        .get(&id)
        .ok_or_else(|| format!("Process '{id}' not found"))?;
    match proc {
        ProcessInstance::Pty(pty) => pty.killer
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("Resize failed: {e}")),
        ProcessInstance::Pipe(_) => Ok(()), // resize is a no-op for pipe processes
    }
}

#[tauri::command]
pub fn kill_pty(state: tauri::State<PtyState>, id: String) -> Result<(), String> {
    let mut map = state.procs.lock();
    if let Some(proc) = map.remove(&id) {
        match proc {
            ProcessInstance::Pty(mut pty) => {
                pty.child.kill().map_err(|e| format!("Kill failed: {e}"))?;
            }
            ProcessInstance::Pipe(mut pipe) => {
                pipe.child.kill().map_err(|e| format!("Kill failed: {e}"))?;
            }
        }
    }
    Ok(())
}

/// Spawn a CLI process in interactive PTY mode with NDJSON output parsing.
///
/// Uses a PTY so the CLI detects a terminal and shows interactive prompts
/// (trust confirmation, channel development warning, etc.). Output is parsed
/// as NDJSON via `--output-format stream-json`. User input is sent as plain
/// text + carriage return (interactive mode, no `--input-format`).
///
/// The reader thread operates in two phases:
///
/// **Phase 1 (Init):** Read raw PTY output byte-by-byte, accumulating into a
/// line buffer. Confirmation prompts from Ink UI are auto-accepted by sending
/// Enter (\r\n). Once a line starting with `{"type":"system"` is detected,
/// transition to Phase 2.
///
/// **Phase 2 (Stream):** Read line-by-line, strip ANSI escapes, parse NDJSON,
/// and emit `chat-message-{id}` Tauri events. PTY echo lines (user input
/// echoed back) are silently dropped as they fail JSON parsing.
#[tauri::command]
pub fn spawn_stream_pty(
    app: AppHandle,
    state: tauri::State<PtyState>,
    command: String,
    args: Vec<String>,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    cli_kind: String,
) -> Result<String, String> {
    let kind = match cli_kind.as_str() {
        "claude" => CliKind::ClaudeCode,
        "codex" => CliKind::Codex,
        other => {
            return Err(format!(
                "Unknown cli_kind: {other}. Expected 'claude' or 'codex'."
            ))
        }
    };

    let pty_system = NativePtySystem::default();
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = pty_system
        .openpty(size)
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    let mut cmd = if cfg!(windows) {
        let mut c = CommandBuilder::new("cmd.exe");
        c.arg("/C");
        c.arg(&command);
        for arg in &args {
            c.arg(arg);
        }
        c
    } else {
        let mut c = CommandBuilder::new(&command);
        for arg in &args {
            c.arg(arg);
        }
        c
    };

    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn '{command}': {e}"))?;

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to get PTY writer: {e}"))?;

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to get PTY reader: {e}"))?;

    let id = Uuid::new_v4().to_string();
    let id_clone = id.clone();
    let id_clone2 = id.clone();
    let app_clone = app.clone();

    // writer_for_init: we use the procs map to access the writer from the
    // background thread (take_writer can only be called once).

    state.procs.lock().insert(
        id.clone(),
        ProcessInstance::Pty(PtyInstance {
            writer,
            child,
            killer: pair.master,
        }),
    );

    // Spawn background thread: Phase 1 (init) → Phase 2 (stream)
    let procs_clone = Arc::clone(&state.procs);
    std::thread::spawn(move || {
        // ── Phase 1: Auto-confirm interactive prompts ──
        // Read byte-by-byte, detect prompts, send Enter.
        // Transition when we see the first JSON line.

        // Timer-based auto-confirm: Ink UI renders without newline characters,
        // so the byte-by-byte pattern matching may never fire. This timer
        // thread periodically sends Enter (\r) to dismiss any confirmation
        // prompts during Phase 1 init. (#57)
        let init_done = Arc::new(AtomicBool::new(false));
        let init_done_timer = Arc::clone(&init_done);
        let procs_for_timer = Arc::clone(&procs_clone);
        let id_for_timer = id_clone2.clone();
        std::thread::spawn(move || {
            for _ in 0..15 {
                std::thread::sleep(Duration::from_secs(2));
                if init_done_timer.load(Ordering::Relaxed) {
                    return;
                }
                let mut map = procs_for_timer.lock();
                if let Some(ProcessInstance::Pty(pty)) = map.get_mut(&id_for_timer) {
                    let _ = pty.writer.write_all(b"\r");
                    let _ = pty.writer.flush();
                }
            }
        });

        let mut init_buf = Vec::with_capacity(8192);
        let mut byte = [0u8; 1];
        let mut json_start = None;

        'init: loop {
            match reader.read(&mut byte) {
                Ok(0) => break,
                Ok(1) => {
                    init_buf.push(byte[0]);

                    // Check if current buffer ends with a line
                    if byte[0] == b'\n' || init_buf.len() > 4096 {
                        let line = String::from_utf8_lossy(&init_buf);
                        let stripped = stream_parser::strip_ansi(&line);
                        let trimmed = stripped.trim();

                        // Detect JSON start → transition to Phase 2
                        if trimmed.starts_with('{') {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                                // Found first JSON line, stop timer thread and switch to Phase 2
                                init_done.store(true, Ordering::Relaxed);
                                json_start = Some(v);
                                break 'init;
                            }
                        }

                        // Auto-confirm: Ink UI renders selection prompts.
                        // When we see "> 1." (selected option), send Enter.
                        if trimmed.contains("> 1.") || trimmed.contains("Enter to confirm") {
                            let mut map = procs_clone.lock();
                            if let Some(ProcessInstance::Pty(pty)) = map.get_mut(&id_clone2) {
                                let _ = pty.writer.write_all(b"\r\n");
                                let _ = pty.writer.flush();
                            }
                            drop(map);
                        }

                        init_buf.clear();
                    }
                }
                _ => break,
            }
        }

        // Process the first JSON value if we found one
        if let Some(value) = &json_start {
            let messages = stream_parser::convert_claude_all(value);
            if !messages.is_empty() && kind == CliKind::ClaudeCode {
                for msg in messages {
                    let _ = app_clone.emit(&format!("chat-message-{}", id_clone), &msg);
                }
            } else if let Some(msg) = stream_parser::convert(kind, value) {
                let _ = app_clone.emit(&format!("chat-message-{}", id_clone), &msg);
            }
            let _ = app_clone.emit(&format!("stream-raw-{}", id_clone), value);
        }

        // ── Phase 2: Stream NDJSON (or ANSI in interactive/Channel mode) ──
        // Wrap reader in BufReader for line-by-line reading.
        // Note: PTY echo lines (user input echoed back by the terminal) are
        // plain text and will fail JSON parsing in parse_ndjson_line, so they
        // are silently dropped. No explicit filtering needed.
        let buf_reader = BufReader::new(reader);
        for line in buf_reader.lines() {
            match line {
                Ok(text) => {
                    // Try JSON first (stream-json mode)
                    if let Some(value) = stream_parser::parse_ndjson_line(&text) {
                        let messages = stream_parser::convert_claude_all(&value);
                        if !messages.is_empty() && kind == CliKind::ClaudeCode {
                            for msg in messages {
                                let _ = app_clone.emit(&format!("chat-message-{}", id_clone), &msg);
                            }
                        } else if let Some(msg) = stream_parser::convert(kind, &value) {
                            let _ = app_clone.emit(&format!("chat-message-{}", id_clone), &msg);
                        }
                        let _ = app_clone.emit(&format!("stream-raw-{}", id_clone), &value);
                    } else {
                        // Not JSON — check for Channel push in ANSI output
                        if let Some(push_msg) = stream_parser::detect_channel_push(&text) {
                            let _ = app_clone.emit(&format!("chat-message-{}", id_clone), &push_msg);
                        }
                    }
                }
                Err(_) => break,
            }
        }

        // Collect exit code
        let exit_code: Option<u32> = {
            let mut map = procs_clone.lock();
            if let Some(ProcessInstance::Pty(pty)) = map.get_mut(&id_clone2) {
                pty.child.wait().ok().map(|status| status.exit_code())
            } else {
                None
            }
        };
        let _ = app_clone.emit(&format!("pty-exit-{}", id_clone2), exit_code);
    });

    Ok(id)
}

/// Spawn a CLI process in pipe mode for non-interactive CLIs (e.g. Codex exec).
///
/// Uses stdin/stdout pipes (no PTY). Reads stdout line-by-line, parses NDJSON,
/// and emits `chat-message-{id}` Tauri events. Stdin is closed immediately
/// (Codex exec takes the prompt via CLI args, not stdin).
#[tauri::command]
pub fn spawn_stream_pipe(
    app: AppHandle,
    state: tauri::State<PtyState>,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    cli_kind: String,
) -> Result<String, String> {
    let kind = match cli_kind.as_str() {
        "claude" => CliKind::ClaudeCode,
        "codex" => CliKind::Codex,
        other => {
            return Err(format!(
                "Unknown cli_kind: {other}. Expected 'claude' or 'codex'."
            ))
        }
    };

    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd.exe");
        c.arg("/C");
        c.arg(&command);
        for arg in &args {
            c.arg(arg);
        }
        c
    } else {
        let mut c = std::process::Command::new(&command);
        for arg in &args {
            c.arg(arg);
        }
        c
    };

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn '{command}': {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout".to_string())?;

    let id = Uuid::new_v4().to_string();
    let id_clone = id.clone();
    let id_clone2 = id.clone();
    let app_clone = app.clone();

    state.procs.lock().insert(
        id.clone(),
        ProcessInstance::Pipe(PipeInstance {
            stdin: child.stdin.take().unwrap_or_else(|| {
                // stdin was set to null; create a placeholder that will never be used.
                // PipeInstance requires a ChildStdin field for write_pty compatibility.
                std::process::Command::new(if cfg!(windows) { "cmd.exe" } else { "true" })
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .expect("placeholder stdin spawn")
                    .stdin
                    .take()
                    .expect("placeholder stdin")
            }),
            child,
        }),
    );

    // Background reader thread: line-by-line NDJSON parsing
    let procs_clone = Arc::clone(&state.procs);
    std::thread::spawn(move || {
        let buf_reader = BufReader::new(stdout);
        for line in buf_reader.lines() {
            match line {
                Ok(text) => {
                    if let Some(value) = stream_parser::parse_ndjson_line(&text) {
                        if let Some(msg) = stream_parser::convert(kind, &value) {
                            let _ = app_clone.emit(&format!("chat-message-{}", id_clone), &msg);
                        }
                        let _ = app_clone.emit(&format!("stream-raw-{}", id_clone), &value);
                    }
                }
                Err(_) => break,
            }
        }

        // Collect exit code
        let exit_code: Option<u32> = {
            let mut map = procs_clone.lock();
            if let Some(ProcessInstance::Pipe(pipe)) = map.get_mut(&id_clone2) {
                pipe.child.wait().ok().map(|s| s.code().unwrap_or(1) as u32)
            } else {
                None
            }
        };
        let _ = app_clone.emit(&format!("pty-exit-{}", id_clone2), exit_code);
    });

    Ok(id)
}
