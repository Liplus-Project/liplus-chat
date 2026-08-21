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

/// Where the sidecar entry point lives.
///
/// Stage-one distribution runs from the repository, so the walk up from the
/// working directory is the normal path; the env override exists for a layout
/// this does not predict.
fn resolve_sidecar_entry() -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var("LIPLUS_SIDECAR_ENTRY") {
        let path = PathBuf::from(&explicit);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("LIPLUS_SIDECAR_ENTRY points at a missing file: {explicit}"));
    }

    let mut dir = std::env::current_dir()
        .map_err(|e| format!("Failed to resolve the working directory: {e}"))?;
    for _ in 0..4 {
        let candidate = dir.join("sidecar").join("src").join("index.ts");
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    Err("Could not find sidecar/src/index.ts. Set LIPLUS_SIDECAR_ENTRY to its path.".to_string())
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

    let sidecar_entry = resolve_sidecar_entry()?;
    let mcp_config = register_sidecar(
        &cwd,
        &RoomRegistration {
            room_url: &room_url,
            token: &room.token(),
            agent_name: &tab.name,
            sidecar_entry: &sidecar_entry,
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
