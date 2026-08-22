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
use mcp_config::{
    channel_launch_args, register_sidecar, reject_incompatible_flags, server_name_for,
    RoomRegistration,
};
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
///
/// The entry names this session's own server, which is a function of the name
/// being launched under, so the preview moves as the name field is typed in.
/// That is the point rather than a side effect: the identity a session is about
/// to take is the thing this launch is now choosing.
#[tauri::command]
pub fn preview_launch_args(args: Vec<String>, name: String) -> Vec<String> {
    channel_launch_args(&args, &server_name_for(name.trim()))
}

/// What the caller gets back after a session joins.
#[derive(Debug, serde::Serialize)]
pub struct StartedSession {
    pub pty_id: String,
    /// Absolute path of the `.mcp.json` this touched, so the UI can say where.
    pub mcp_config: String,
    /// When the PTY was spawned, RFC 3339.
    ///
    /// Stamped here rather than on the screen because this is the moment the
    /// session began: the screen learns of it after the launch has returned,
    /// and a launch that takes a while would be recorded as having started
    /// late. Same clock as a post's `ts`, so the panel's start time and the
    /// first line of the conversation can be read against each other.
    pub started_at: String,
}

/// Launch one session under a declared identity.
///
/// `name` and `hue` are the session's own, not the tab's. A tab is which CLI to
/// run; who joins the room is chosen at the moment of joining, the same way the
/// screen's person chooses theirs. Under the tab-attribute form every launch
/// answered to the one name in the default config, so two sessions were both
/// `Claude Code` and neither could be addressed (#40).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn start_session(
    app: AppHandle,
    room: tauri::State<RoomState>,
    pty_state: tauri::State<PtyState>,
    tab: TabConfig,
    name: String,
    hue: Option<f64>,
    cols: u16,
    rows: u16,
) -> Result<StartedSession, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(
            "This session has no name. Give it one before starting: it is what the room \
             lists it under and what a post is addressed to."
                .to_string(),
        );
    }

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
    // Keyed on the name, so two sessions launched into one working directory
    // write two entries instead of overwriting each other's identity (#40).
    let server_name = server_name_for(&name);
    let mcp_config = register_sidecar(
        &cwd,
        &RoomRegistration {
            room_url: &room_url,
            token: &room.token(),
            agent_name: &name,
            agent_hue: hue,
            sidecar_entry: &sidecar_entry,
            sidecar_runner: &sidecar_runner,
        },
    )?;

    let started_at = crate::room::now_iso();
    let pty_id = pty::spawn_pty(
        app,
        pty_state,
        tab.command.clone(),
        channel_launch_args(&tab.args, &server_name),
        cols,
        rows,
        Some(cwd.to_string_lossy().to_string()),
    )?;

    Ok(StartedSession {
        pty_id,
        mcp_config: mcp_config.to_string_lossy().to_string(),
        started_at,
    })
}
