//! Registering the room sidecar in a project's `.mcp.json`, and the flag
//! guard that keeps a launched session able to receive channel pushes.
//!
//! Both are conditions the round trip does not survive without (see the
//! 成立条件 in `docs/0-requirements.md`):
//!
//!   - The sidecar must be registered **by name**. A config handed over with
//!     `--mcp-config` does not resolve on the channel side.
//!   - The launch must carry `--dangerously-load-development-channels
//!     server:<name>` and nothing else on that axis. Adding `--channels`
//!     registers the same server twice and takes the whole room down.
//!
//! This crate holds no tauri: it writes into the user's own project directory,
//! which is the part of liplus-chat that most needs test coverage, and a test
//! binary linking the tauri tree does not load on the GNU target.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The name the sidecar is registered under in `.mcp.json`. The launch flag
/// carries the same name (`server:<name>`), so the two must not drift apart.
pub const SERVER_NAME: &str = "liplus-chat-room";

/// Flags that silently stop channel pushes from arriving.
pub const INCOMPATIBLE_FLAGS: &[&str] =
    &["--channels", "--print", "--input-format", "--output-format"];

/// What a session launch needs to know about the room it is joining.
#[derive(Debug, Clone)]
pub struct RoomRegistration<'a> {
    /// `ws://127.0.0.1:<port>` of the room socket.
    pub room_url: &'a str,
    /// Bearer token the sidecar must present.
    pub token: &'a str,
    /// Display name this session speaks under.
    pub agent_name: &'a str,
    /// Absolute path of the sidecar entry point.
    pub sidecar_entry: &'a Path,
}

/// Reject a launch whose flags would leave the session unable to hear the room.
///
/// Rejected rather than stripped: a session that launches with the flag quietly
/// removed looks like it worked, and the failure then surfaces as silence.
/// Returns the offending flag.
pub fn reject_incompatible_flags(args: &[String]) -> Result<(), &'static str> {
    for arg in args {
        // `--flag=value` counts; matching the bare flag alone would miss it.
        let head = arg.split('=').next().unwrap_or(arg);
        if let Some(found) = INCOMPATIBLE_FLAGS.iter().find(|flag| **flag == head) {
            return Err(found);
        }
    }
    Ok(())
}

/// The launch arguments for a channel-enabled session, given the tab's own.
pub fn channel_launch_args(base: &[String]) -> Vec<String> {
    let mut args = base.to_vec();
    args.push("--dangerously-load-development-channels".to_string());
    args.push(format!("server:{SERVER_NAME}"));
    args
}

/// How the CLI should spawn the sidecar, as a `.mcp.json` command and args.
///
/// npx resolves through PATHEXT on Windows only when a shell runs it.
fn spawn_form(entry: &str) -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd",
            vec![
                "/c".to_string(),
                "npx".to_string(),
                "tsx".to_string(),
                entry.to_string(),
            ],
        )
    } else {
        ("npx", vec!["tsx".to_string(), entry.to_string()])
    }
}

/// Merge the room server into the `.mcp.json` at `dir`, preserving whatever
/// else is there. Returns the path written.
///
/// This writes into the user's own project directory, because project scope is
/// where a Claude Code MCP server is normally registered. Only this one key is
/// touched; existing servers and unrelated top-level keys survive verbatim.
pub fn register_sidecar(dir: &Path, room: &RoomRegistration<'_>) -> Result<PathBuf, String> {
    let entry = room.sidecar_entry.to_string_lossy().to_string();
    let (command, args) = spawn_form(&entry);

    let path = dir.join(".mcp.json");
    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        // Truncating a file that failed to parse would destroy whatever it held.
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
                "LIPLUS_ROOM_URL": room.room_url,
                "LIPLUS_ROOM_TOKEN": room.token,
                "LIPLUS_AGENT_NAME": room.agent_name,
                "LIPLUS_ROOM_ID": "liplus-chat",
            },
        }),
    );

    let text = serde_json::to_string_pretty(&root)
        .map_err(|e| format!("Failed to serialize {}: {e}", path.display()))?;
    std::fs::write(&path, text).map_err(|e| format!("Failed to write {}: {e}", path.display()))?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("liplus-chat-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Scratch(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn registration<'a>(entry: &'a Path) -> RoomRegistration<'a> {
        RoomRegistration {
            room_url: "ws://127.0.0.1:1234",
            token: "tok",
            agent_name: "Lin",
            sidecar_entry: entry,
        }
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("parse")
    }

    #[test]
    fn registers_the_room_server_when_no_config_exists() {
        let scratch = Scratch::new();
        let entry = PathBuf::from("sidecar/src/index.ts");
        let path = register_sidecar(scratch.path(), &registration(&entry)).expect("register");

        let json = read(&path);
        let server = &json["mcpServers"][SERVER_NAME];
        assert_eq!(server["env"]["LIPLUS_ROOM_URL"], "ws://127.0.0.1:1234");
        assert_eq!(server["env"]["LIPLUS_ROOM_TOKEN"], "tok");
        assert_eq!(server["env"]["LIPLUS_AGENT_NAME"], "Lin");
        assert_eq!(server["env"]["LIPLUS_ROOM_ID"], "liplus-chat");

        let args = server["args"].as_array().expect("args");
        let last = args.last().expect("entry argument");
        assert_eq!(last, "sidecar/src/index.ts");
    }

    #[test]
    fn leaves_everything_else_in_the_config_alone() {
        // This writes into the user's own project directory. Clobbering a
        // server they configured themselves is the failure that matters here.
        let scratch = Scratch::new();
        std::fs::write(
            scratch.path().join(".mcp.json"),
            r#"{"mcpServers":{"theirs":{"command":"their-server"}},"unrelated":42}"#,
        )
        .expect("seed");

        let entry = PathBuf::from("sidecar/src/index.ts");
        let path = register_sidecar(scratch.path(), &registration(&entry)).expect("register");

        let json = read(&path);
        assert_eq!(json["mcpServers"]["theirs"]["command"], "their-server");
        assert_eq!(json["unrelated"], 42);
        assert!(json["mcpServers"][SERVER_NAME].is_object());
    }

    #[test]
    fn re_registering_replaces_only_its_own_entry() {
        let scratch = Scratch::new();
        let entry = PathBuf::from("sidecar/src/index.ts");

        register_sidecar(scratch.path(), &registration(&entry)).expect("first");
        let second = RoomRegistration {
            room_url: "ws://127.0.0.1:9999",
            token: "tok2",
            agent_name: "Lay",
            sidecar_entry: &entry,
        };
        let path = register_sidecar(scratch.path(), &second).expect("second");

        let json = read(&path);
        let server = &json["mcpServers"][SERVER_NAME];
        assert_eq!(server["env"]["LIPLUS_ROOM_URL"], "ws://127.0.0.1:9999");
        assert_eq!(server["env"]["LIPLUS_AGENT_NAME"], "Lay");
        assert_eq!(
            json["mcpServers"].as_object().expect("servers").len(),
            1,
            "re-registering must not accumulate entries"
        );
    }

    #[test]
    fn refuses_to_overwrite_a_config_it_cannot_parse() {
        let scratch = Scratch::new();
        let seeded = "{ not json";
        std::fs::write(scratch.path().join(".mcp.json"), seeded).expect("seed");

        let entry = PathBuf::from("sidecar/src/index.ts");
        let err = register_sidecar(scratch.path(), &registration(&entry)).unwrap_err();
        assert!(err.contains("not valid JSON"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read_to_string(scratch.path().join(".mcp.json")).expect("read"),
            seeded,
            "the unparseable file must be left as it was"
        );
    }

    #[test]
    fn rejects_flags_that_stop_channel_pushes() {
        for arg in ["--channels", "--print", "--input-format", "--output-format"] {
            let args = vec!["--verbose".to_string(), arg.to_string()];
            assert_eq!(reject_incompatible_flags(&args), Err(arg));
        }
    }

    #[test]
    fn rejects_an_incompatible_flag_written_with_an_equals_sign() {
        let args = vec!["--output-format=stream-json".to_string()];
        assert_eq!(reject_incompatible_flags(&args), Err("--output-format"));
    }

    #[test]
    fn accepts_arguments_that_do_not_touch_that_axis() {
        let args = vec!["--verbose".to_string(), "--model=opus".to_string()];
        assert_eq!(reject_incompatible_flags(&args), Ok(()));
    }

    #[test]
    fn the_launch_flag_names_the_server_the_config_registers() {
        // The flag and the `.mcp.json` key are one fact in two places; a drift
        // between them fails as a room that never receives anything.
        let args = channel_launch_args(&["--verbose".to_string()]);
        assert_eq!(
            args,
            vec![
                "--verbose".to_string(),
                "--dangerously-load-development-channels".to_string(),
                format!("server:{SERVER_NAME}"),
            ]
        );
    }
}
