//! Putting one CLI session into the room.
//!
//! The conditions the round trip does not survive without — registration by
//! name in `.mcp.json`, and the launch flag carried alone — live in the
//! `mcp-config` crate, which holds no tauri so that they can be tested. What
//! stays here is the part that needs the app: the room's port and token, the
//! working directory, and the PTY the session is held open on.
//!
//! The session must be interactive: `--print` never receives a push.

use crate::config::TabConfig;
use crate::pty::{self, PtyState};
use crate::room::RoomState;
use mcp_config::{channel_launch_args, register_sidecar, reject_incompatible_flags, RoomRegistration};
use std::path::PathBuf;
use tauri::AppHandle;

/// The sidecar entry point and the runner that executes it.
///
/// Both come from one walk, and both are absolute. The CLI spawns the sidecar
/// from the user's own project directory, so a path resolved by name there
/// resolves against their tree, not ours (#22).
///
/// Stage-one distribution runs from the repository, so the walk up from the
/// working directory is the normal path; the env overrides exist for a layout
/// this does not predict.
fn resolve_sidecar_paths() -> Result<(PathBuf, PathBuf), String> {
    fn from_env(key: &str) -> Result<Option<PathBuf>, String> {
        match std::env::var(key) {
            Err(_) => Ok(None),
            Ok(value) => {
                let path = PathBuf::from(&value);
                if path.is_file() {
                    Ok(Some(path))
                } else {
                    Err(format!("{key} points at a missing file: {value}"))
                }
            }
        }
    }

    let entry_override = from_env("LIPLUS_SIDECAR_ENTRY")?;
    let runner_override = from_env("LIPLUS_SIDECAR_RUNNER")?;
    if let (Some(entry), Some(runner)) = (&entry_override, &runner_override) {
        return Ok((entry.clone(), runner.clone()));
    }

    let mut dir = std::env::current_dir()
        .map_err(|e| format!("Failed to resolve the working directory: {e}"))?;
    for _ in 0..4 {
        let entry = dir.join("sidecar").join("src").join("index.ts");
        let runner = dir
            .join("node_modules")
            .join("tsx")
            .join("dist")
            .join("cli.mjs");
        if entry.is_file() && runner.is_file() {
            return Ok((
                entry_override.unwrap_or(entry),
                runner_override.unwrap_or(runner),
            ));
        }
        if !dir.pop() {
            break;
        }
    }
    Err("Could not find sidecar/src/index.ts next to node_modules/tsx. \
         Run npm install, or set LIPLUS_SIDECAR_ENTRY and LIPLUS_SIDECAR_RUNNER."
        .to_string())
}

/// Split a launch-options string into arguments.
///
/// The splitter lives in `mcp-config` so it is covered by tests; this is the
/// door the frontend reaches it through, rather than a second implementation
/// in TypeScript that would drift from the tested one.
#[tauri::command]
pub fn parse_launch_options(text: String) -> Vec<String> {
    mcp_config::split_launch_options(&text)
}

/// The arguments a launch would actually use, for display.
///
/// The app merges its own channel entry into what the person wrote, so the
/// line they typed is not the line that runs. This returns the line that runs.
#[tauri::command]
pub fn preview_launch_args(args: Vec<String>) -> Vec<String> {
    channel_launch_args(&args)
}

/// What the caller gets back after a session joins.
#[derive(Debug, serde::Serialize)]
pub struct StartedSession {
    pub pty_id: String,
    /// Absolute path of the `.mcp.json` this touched, so the UI can say where.
    pub mcp_config: String,
}

#[tauri::command]
pub fn start_session(
    app: AppHandle,
    room: tauri::State<RoomState>,
    pty_state: tauri::State<PtyState>,
    tab: TabConfig,
    cols: u16,
    rows: u16,
) -> Result<StartedSession, String> {
    if let Err(flag) = reject_incompatible_flags(&tab.args) {
        return Err(format!(
            "Tab \"{}\" passes {flag}, which stops channel pushes from arriving. \
             Remove it from the tab configuration.",
            tab.name
        ));
    }

    let port = room
        .port()
        .ok_or_else(|| "The room socket is not listening yet.".to_string())?;
    let room_url = format!("ws://127.0.0.1:{port}");

    // No fallback to the app's own process directory. Under `tauri dev` that
    // is `src-tauri`, and a session silently launched there is a session the
    // person never chose and cannot see they got (#20).
    let cwd = match tab.cwd.as_deref().map(str::trim) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            return Err(format!(
                "Tab \"{}\" has no working directory. Set one before starting a session.",
                tab.name
            ))
        }
    };
    if !cwd.is_dir() {
        return Err(format!("Tab \"{}\" points at a missing directory: {}", tab.name, cwd.display()));
    }

    let (sidecar_entry, sidecar_runner) = resolve_sidecar_paths()?;
    let mcp_config = register_sidecar(
        &cwd,
        &RoomRegistration {
            room_url: &room_url,
            token: &room.token(),
            agent_name: &tab.name,
            sidecar_entry: &sidecar_entry,
            sidecar_runner: &sidecar_runner,
        },
    )?;

    let pty_id = pty::spawn_pty(
        app,
        pty_state,
        tab.command.clone(),
        channel_launch_args(&tab.args),
        cols,
        rows,
        Some(cwd.to_string_lossy().to_string()),
    )?;

    Ok(StartedSession {
        pty_id,
        mcp_config: mcp_config.to_string_lossy().to_string(),
    })
}
