//! The room socket.
//!
//! The app hosts; the sidecars connect. That direction is forced: the CLI
//! spawns its MCP servers itself, so the app never learns the launch moment or
//! a port chosen on that side. See `docs/0-requirements.md`.
//!
//! Frames on the wire are the room protocol:
//!
//!   room    -> sidecar : { type: "say",   message_id, user, content, to?, ts }
//!   sidecar -> room    : { type: "hello", protocol, agent }
//!                        { type: "reply", message_id, agent, content, to?, ts }
//!
//! `to` is optional in both directions and carries the same vocabulary: the
//! display name of the participant addressed. The room still fans every frame
//! out to every sidecar — narrowing delivery here would make the room hold who
//! heard what, and answering is the agent's judgment, not the room's.
//!
//! Everything the frontend needs arrives as a `room-message` event. The room
//! never reads a CLI's terminal output; that is not a message source.

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// Bumped when a frame's shape changes in a way a sidecar must notice.
pub const PROTOCOL_VERSION: u32 = 1;

/// One line of the room, as the frontend sees it.
#[derive(Debug, Clone, Serialize)]
pub struct RoomMessage {
    pub message_id: String,
    /// "human" for a person, "agent" for a session.
    pub kind: String,
    /// Display name of the speaker.
    pub speaker: String,
    pub content: String,
    pub to: Option<String>,
    pub ts: String,
}

#[derive(Debug, Deserialize)]
struct AgentFrame {
    #[serde(rename = "type")]
    kind: String,
    message_id: Option<String>,
    agent: Option<String>,
    content: Option<String>,
    to: Option<String>,
    ts: Option<String>,
    protocol: Option<u32>,
}

struct RoomInner {
    port: Option<u16>,
    agents: BTreeSet<String>,
}

/// Shared handle to the room socket. Cloneable; all clones share one room.
#[derive(Clone)]
pub struct RoomState {
    inner: Arc<Mutex<RoomInner>>,
    /// Frames fanned out to every connected sidecar.
    to_agents: broadcast::Sender<String>,
    /// Bearer token the sidecar must present. Generated per app run, handed to
    /// the sidecar through `.mcp.json` env, never written anywhere else.
    token: String,
}

impl RoomState {
    pub fn new() -> Self {
        let (to_agents, _) = broadcast::channel(256);
        RoomState {
            inner: Arc::new(Mutex::new(RoomInner {
                port: None,
                agents: BTreeSet::new(),
            })),
            to_agents,
            token: Uuid::new_v4().to_string(),
        }
    }

    pub fn token(&self) -> String {
        self.token.clone()
    }

    pub fn port(&self) -> Option<u16> {
        self.inner.lock().port
    }

    pub fn agents(&self) -> Vec<String> {
        self.inner.lock().agents.iter().cloned().collect()
    }

    fn set_port(&self, port: u16) {
        self.inner.lock().port = Some(port);
    }

    fn add_agent(&self, name: &str) {
        self.inner.lock().agents.insert(name.to_string());
    }

    fn remove_agent(&self, name: &str) {
        self.inner.lock().agents.remove(name);
    }
}

fn now_iso() -> String {
    // Tauri already pulls chrono-free time handling in; a plain RFC3339-ish
    // stamp from SystemTime keeps the dependency list unchanged.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    // Days-since-epoch to calendar date, civil-from-days (Howard Hinnant).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60,
        millis
    )
}

/// Bind the room socket and start accepting sidecars.
///
/// Port 0: the OS picks. The port is handed to sidecars through `.mcp.json`,
/// so nothing needs it to be stable across runs.
pub async fn start(app: AppHandle, room: RoomState) -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Failed to bind room socket: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed to read room socket address: {e}"))?
        .port();
    room.set_port(port);

    // The frontend loads before this bind completes, so a poll at load time
    // reads "not listening" and reports a failure that is only a race. The
    // event is the authority; `room_port` remains for a late reader.
    let _ = app.emit("room-ready", port);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let app = app.clone();
            let room = room.clone();
            tokio::spawn(async move {
                if let Err(err) = serve_agent(app, room, stream).await {
                    eprintln!("[room] agent connection ended: {err}");
                }
            });
        }
    });

    Ok(port)
}

async fn serve_agent(
    app: AppHandle,
    room: RoomState,
    stream: tokio::net::TcpStream,
) -> Result<(), String> {
    let expected = format!("Bearer {}", room.token());
    // The listener is on loopback, but any local process can reach loopback.
    // The token is what makes this room, and not merely this machine.
    let check = |req: &Request, res: Response| -> Result<Response, ErrorResponse> {
        let ok = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v == expected)
            .unwrap_or(false);
        if ok {
            Ok(res)
        } else {
            let mut deny = ErrorResponse::new(Some("unauthorized".to_string()));
            *deny.status_mut() = StatusCode::UNAUTHORIZED;
            Err(deny)
        }
    };

    let ws = tokio_tungstenite::accept_hdr_async(stream, check)
        .await
        .map_err(|e| format!("handshake failed: {e}"))?;
    let (mut sink, mut source) = ws.split();

    let mut from_room = room.to_agents.subscribe();
    let pump = tokio::spawn(async move {
        while let Ok(frame) = from_room.recv().await {
            if sink.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
    });

    // Known once the sidecar says hello; used to drop it from the roster.
    let mut agent_name: Option<String> = None;

    while let Some(Ok(msg)) = source.next().await {
        let Message::Text(text) = msg else { continue };
        let Ok(frame) = serde_json::from_str::<AgentFrame>(&text) else {
            continue;
        };

        match frame.kind.as_str() {
            "hello" => {
                let name = frame.agent.unwrap_or_else(|| "agent".to_string());
                if frame.protocol != Some(PROTOCOL_VERSION) {
                    // Legible mismatch beats a silent half-working room.
                    eprintln!(
                        "[room] \"{name}\" speaks protocol {:?}, room speaks {PROTOCOL_VERSION}",
                        frame.protocol
                    );
                }
                room.add_agent(&name);
                agent_name = Some(name);
                let _ = app.emit("room-agents", room.agents());
            }
            "reply" => {
                let speaker = frame
                    .agent
                    .or_else(|| agent_name.clone())
                    .unwrap_or_else(|| "agent".to_string());
                let message = RoomMessage {
                    message_id: frame.message_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                    kind: "agent".to_string(),
                    speaker,
                    content: frame.content.unwrap_or_default(),
                    to: frame.to,
                    ts: frame.ts.unwrap_or_else(now_iso),
                };
                let _ = app.emit("room-message", message);
            }
            _ => {}
        }
    }

    pump.abort();
    if let Some(name) = agent_name {
        room.remove_agent(&name);
        let _ = app.emit("room-agents", room.agents());
    }
    Ok(())
}

// ── Commands ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn room_port(state: tauri::State<RoomState>) -> Option<u16> {
    state.port()
}

#[tauri::command]
pub fn room_agents(state: tauri::State<RoomState>) -> Vec<String> {
    state.agents()
}

/// Post a human utterance into the room.
///
/// The message is echoed back to the frontend through the same `room-message`
/// event the agents' replies use, so the room has one ordering authority
/// rather than two.
#[tauri::command]
pub fn room_say(
    app: AppHandle,
    state: tauri::State<RoomState>,
    user: String,
    content: String,
    to: Option<String>,
) -> Result<String, String> {
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err("content is empty".to_string());
    }

    // Absent is the key omitted, never an empty one: an agent matching `to`
    // against its own name must not have to rule "" out first. Normalising
    // here keeps that shape a property of the room rather than of its callers.
    let to = to
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());

    let message_id = Uuid::new_v4().to_string();
    let ts = now_iso();

    let mut frame = serde_json::json!({
        "type": "say",
        "message_id": message_id,
        "user": user,
        "content": content,
        "ts": ts,
    });
    // Omitted rather than null when absent, matching the `reply` direction.
    if let Some(name) = &to {
        frame["to"] = serde_json::Value::String(name.clone());
    }

    // No subscribers means no session has joined yet. That is not an error —
    // the room accepts what is said in it; a later joiner simply missed it.
    let _ = state.to_agents.send(frame.to_string());

    let _ = app.emit(
        "room-message",
        RoomMessage {
            message_id: message_id.clone(),
            kind: "human".to_string(),
            speaker: user,
            content,
            to,
            ts,
        },
    );

    Ok(message_id)
}
