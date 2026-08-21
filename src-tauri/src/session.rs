//! Putting one CLI session into the room.
//!
//! Two things have to be true at once, and both are load-bearing (see the
//! 成立条件 in `docs/0-requirements.md`):
//!
//!   1. The sidecar is registered by name in `.mcp.json`. A config handed over
//!      with `--mcp-config` does not resolve on the channel side.
//!   2. The CLI is launched with `--dangerously-load-development-channels
//!      server:<name>` and nothing else on that axis. Adding `--channels`
//!      registers the same server twice and takes the whole room down.
//!
//! And the session must be interactive — `--print` never receives a push — so
//! it is held open on a PTY.

use crate::config::TabConfig;
use crate::pty::{self, PtyState};
use crate::room::{RoomState, SERVER_NAME};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use tauri::AppHandle;

/// Flags that silently break the channel. Rejected rather than stripped: a
/// session that launches with the flag quietly removed looks like it worked.
const INCOMPATIBLE_FLAGS: &[&str] = &["--channels", "--print", "--input-format", "--output-format"];

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

/// Merge the room server into the `.mcp.json` at `cwd`, preserving whatever
/// else is there.
///
/// This writes into the user's own project directory, because project scope is
/// where a Claude Code MCP server is normally registered. Only this one key is
/// touched; existing servers and unrelated top-level keys survive verbatim.
fn register_sidecar(
    cwd: &Path,
    room_url: &str,
    token: &str,
    agent_name: &str,
) -> Result<PathBuf, String> {
    let entry = resolve_sidecar_entry()?;
    let entry = entry.to_string_lossy().to_string();

    // npx resolves through PATHEXT on Windows only when a shell runs it.
    let (command, args) = if cfg!(windows) {
        ("cmd", vec!["/c", "npx", "tsx", entry.as_str()])
    } else {
        ("npx", vec!["tsx", entry.as_str()])
    };

    let path = cwd.join(".mcp.json");
    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} exists but is not valid JSON ({e}). Fix or move it before starting a session.",
                path.display()
            )
        })?
    } else {
        Value::Object(Map::new())
    };

    if !root.is_object() {
        return Err(format!("{} is not a JSON object.", path.display()));
    }
    let obj = root.as_object_mut().expect("checked above");
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    if !servers.is_object() {
        return Err(format!("{} has a non-object mcpServers.", path.display()));
    }

    servers.as_object_mut().expect("checked above").insert(
        SERVER_NAME.to_string(),
        json!({
            "command": command,
            "args": args,
            "env": {
                "LIPLUS_ROOM_URL": room_url,
                "LIPLUS_ROOM_TOKEN": token,
                "LIPLUS_AGENT_NAME": agent_name,
                "LIPLUS_ROOM_ID": "liplus-chat",
            },
        }),
    );

    let text = serde_json::to_string_pretty(&root)
        .map_err(|e| format!("Failed to serialize {}: {e}", path.display()))?;
    std::fs::write(&path, text).map_err(|e| format!("Failed to write {}: {e}", path.display()))?;

    Ok(path)
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
    for arg in &tab.args {
        // `--flag=value` counts; matching the bare flag alone would miss it.
        let head = arg.split('=').next().unwrap_or(arg);
        if INCOMPATIBLE_FLAGS.contains(&head) {
            return Err(format!(
                "Tab \"{}\" passes {head}, which stops channel pushes from arriving. \
                 Remove it from the tab configuration.",
                tab.name
            ));
        }
    }

    let port = room
        .port()
        .ok_or_else(|| "The room socket is not listening yet.".to_string())?;
    let room_url = format!("ws://127.0.0.1:{port}");

    let cwd = match tab.cwd.as_deref() {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir()
            .map_err(|e| format!("Failed to resolve the working directory: {e}"))?,
    };
    if !cwd.is_dir() {
        return Err(format!("Tab \"{}\" points at a missing directory: {}", tab.name, cwd.display()));
    }

    let mcp_config = register_sidecar(&cwd, &room_url, &room.token(), &tab.name)?;

    let mut args = tab.args.clone();
    args.push("--dangerously-load-development-channels".to_string());
    args.push(format!("server:{SERVER_NAME}"));

    let pty_id = pty::spawn_pty(
        app,
        pty_state,
        tab.command.clone(),
        args,
        cols,
        rows,
        Some(cwd.to_string_lossy().to_string()),
    )?;

    Ok(StartedSession {
        pty_id,
        mcp_config: mcp_config.to_string_lossy().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("liplus-chat-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("parse")
    }

    #[test]
    fn registers_the_room_server_when_no_config_exists() {
        let dir = scratch();
        let path = register_sidecar(&dir, "ws://127.0.0.1:1234", "tok", "Lin").expect("register");

        let json = read(&path);
        let entry = &json["mcpServers"][SERVER_NAME];
        assert_eq!(entry["env"]["LIPLUS_ROOM_URL"], "ws://127.0.0.1:1234");
        assert_eq!(entry["env"]["LIPLUS_ROOM_TOKEN"], "tok");
        assert_eq!(entry["env"]["LIPLUS_AGENT_NAME"], "Lin");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaves_everything_else_in_the_config_alone() {
        // This writes into the user's own project directory. Clobbering a
        // server they configured themselves is the failure that matters here.
        let dir = scratch();
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"theirs":{"command":"their-server"}},"unrelated":42}"#,
        )
        .expect("seed");

        let path = register_sidecar(&dir, "ws://127.0.0.1:1", "tok", "Lay").expect("register");

        let json = read(&path);
        assert_eq!(json["mcpServers"]["theirs"]["command"], "their-server");
        assert_eq!(json["unrelated"], 42);
        assert!(json["mcpServers"][SERVER_NAME].is_object());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_to_overwrite_a_config_it_cannot_parse() {
        // Truncating an unparseable file would destroy whatever it held.
        let dir = scratch();
        let seeded = "{ not json";
        std::fs::write(dir.join(".mcp.json"), seeded).expect("seed");

        let err = register_sidecar(&dir, "ws://127.0.0.1:1", "tok", "Lin").unwrap_err();
        assert!(err.contains("not valid JSON"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read_to_string(dir.join(".mcp.json")).expect("read"),
            seeded
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
