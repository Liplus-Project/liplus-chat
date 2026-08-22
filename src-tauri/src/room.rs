//! The room socket.
//!
//! The app hosts; the sidecars connect. That direction is forced: the CLI
//! spawns its MCP servers itself, so the app never learns the launch moment or
//! a port chosen on that side. See `docs/0-requirements.md`.
//!
//! Frames on the wire are the room protocol:
//!
//!   sidecar -> room : { type: "hello", protocol, name }
//!   both ways       : { type: "post", message_id, speaker, content, to?, ts }
//!
//! One frame kind carries speech, whoever produced it. A person and a session
//! are both participants; what separates them is a name, not a frame. The
//! earlier protocol had `say` for a person and `reply` for a session, and only
//! `say` was ever fanned out — the asymmetry was not a missing line but the
//! shape of the words, so the words went (#39).
//!
//! `to` is optional and carries the display name of the participant addressed.
//! The room still fans every post out to every participant — narrowing
//! delivery here would make the room hold who heard what, and answering is the
//! participant's judgment, not the room's.
//!
//! `speaker` is stamped by the room from the connection the frame arrived on,
//! never read off the frame. A sender cannot claim to be someone else, and the
//! roster and the attribution cannot disagree.
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
///
/// 2: `say` and `reply` collapsed into one `post` frame; `agent` became `name`.
pub const PROTOCOL_VERSION: u32 = 2;

/// One post of the room, as the frontend sees it.
///
/// Carries no participant class. What separates two lines is the name on them.
/// `own` is a self/other axis for display, which is a property of the viewer,
/// not of the speaker.
#[derive(Debug, Clone, Serialize)]
pub struct RoomMessage {
    pub message_id: String,
    /// Display name of the speaker.
    pub speaker: String,
    pub content: String,
    pub to: Option<String>,
    pub ts: String,
    /// True when this screen's own participant produced it.
    pub own: bool,
}

/// A post as it happened, before any viewer-relative framing.
struct Post {
    message_id: String,
    speaker: String,
    content: String,
    to: Option<String>,
    ts: String,
}

/// A post on its way out, tagged with the connection that produced it.
///
/// The tag rides beside the frame, never inside it: it is never serialised, so
/// no sender can supply one and no sender can forge one. Suppression is a
/// property of the connection, which is the one identity a name collision
/// cannot blur (#40).
#[derive(Clone)]
struct Fanout {
    origin: String,
    frame: String,
}

#[derive(Debug, Deserialize)]
struct IncomingFrame {
    #[serde(rename = "type")]
    kind: String,
    message_id: Option<String>,
    name: Option<String>,
    content: Option<String>,
    to: Option<String>,
    ts: Option<String>,
    protocol: Option<u32>,
}

struct RoomInner {
    port: Option<u16>,
    /// Everyone in the room, people and sessions alike.
    participants: BTreeSet<String>,
    /// The name this screen's person currently answers to, so a rename
    /// replaces the roster entry rather than adding a second one.
    local_name: Option<String>,
}

/// Shared handle to the room socket. Cloneable; all clones share one room.
#[derive(Clone)]
pub struct RoomState {
    inner: Arc<Mutex<RoomInner>>,
    /// Posts fanned out to every connected participant but their author.
    to_participants: broadcast::Sender<Fanout>,
    /// Bearer token the sidecar must present. Generated per app run, handed to
    /// the sidecar through `.mcp.json` env, never written anywhere else.
    token: String,
    /// The screen's own connection. It has no socket, so it needs an identity
    /// minted here to sit on the same suppression axis as every other one.
    local_origin: String,
}

impl RoomState {
    pub fn new() -> Self {
        let (to_participants, _) = broadcast::channel(256);
        RoomState {
            inner: Arc::new(Mutex::new(RoomInner {
                port: None,
                participants: BTreeSet::new(),
                local_name: None,
            })),
            to_participants,
            token: Uuid::new_v4().to_string(),
            local_origin: Uuid::new_v4().to_string(),
        }
    }

    pub fn token(&self) -> String {
        self.token.clone()
    }

    pub fn port(&self) -> Option<u16> {
        self.inner.lock().port
    }

    pub fn participants(&self) -> Vec<String> {
        self.inner.lock().participants.iter().cloned().collect()
    }

    fn set_port(&self, port: u16) {
        self.inner.lock().port = Some(port);
    }

    fn add_participant(&self, name: &str) {
        self.inner.lock().participants.insert(name.to_string());
    }

    fn remove_participant(&self, name: &str) {
        self.inner.lock().participants.remove(name);
    }

    /// Seat this screen's person in the room under `name`, replacing an
    /// earlier seat.
    ///
    /// Returns true when the roster changed, so a rename that is not a rename
    /// does not emit a roster event.
    fn seat_local(&self, name: &str) -> bool {
        let mut inner = self.inner.lock();
        if inner.local_name.as_deref() == Some(name) {
            return false;
        }
        if let Some(previous) = inner.local_name.take() {
            inner.participants.remove(&previous);
        }
        inner.participants.insert(name.to_string());
        inner.local_name = Some(name.to_string());
        true
    }
}

/// Absent is the key omitted, never an empty one: a participant matching `to`
/// against their own name must not have to rule the empty string out first.
fn normalize_to(to: Option<String>) -> Option<String> {
    to.map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
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

/// Put one post into the room.
///
/// The single path every utterance takes, whoever spoke. `origin` is the
/// connection it arrived on: the fan-out skips that connection, and the screen
/// reads it to know whether the line is its own. Two callers reach here — the
/// socket loop and the screen's own command — and neither has a path of its
/// own past this point.
fn deliver(app: &AppHandle, room: &RoomState, origin: &str, post: Post) {
    let mut frame = serde_json::json!({
        "type": "post",
        "message_id": post.message_id,
        "speaker": post.speaker,
        "content": post.content,
        "ts": post.ts,
    });
    // Omitted rather than null when absent.
    if let Some(name) = &post.to {
        frame["to"] = serde_json::Value::String(name.clone());
    }

    // No subscribers means no session has joined yet. That is not an error —
    // the room accepts what is said in it; a later joiner simply missed it.
    let _ = room.to_participants.send(Fanout {
        origin: origin.to_string(),
        frame: frame.to_string(),
    });

    let _ = app.emit(
        "room-message",
        RoomMessage {
            message_id: post.message_id,
            speaker: post.speaker,
            content: post.content,
            to: post.to,
            ts: post.ts,
            own: origin == room.local_origin,
        },
    );
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
                if let Err(err) = serve_participant(app, room, stream).await {
                    eprintln!("[room] participant connection ended: {err}");
                }
            });
        }
    });

    Ok(port)
}

async fn serve_participant(
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

    // This connection's identity, minted here. Nothing the far side sends can
    // set it or read it, so nothing the far side sends can wear another
    // participant's suppression or shed its own.
    let origin = Uuid::new_v4().to_string();

    let mut from_room = room.to_participants.subscribe();
    let own_origin = origin.clone();
    let pump = tokio::spawn(async move {
        while let Ok(fanout) = from_room.recv().await {
            // A participant does not receive their own post. Decided on the
            // connection, never on the name: while two participants share a
            // name, a name test drops the other one's posts too (#40).
            if fanout.origin == own_origin {
                continue;
            }
            if sink.send(Message::Text(fanout.frame.into())).await.is_err() {
                break;
            }
        }
    });

    // Known once the participant says hello; used to attribute posts and to
    // drop them from the roster.
    let mut joined_as: Option<String> = None;

    while let Some(Ok(msg)) = source.next().await {
        let Message::Text(text) = msg else { continue };
        let Ok(frame) = serde_json::from_str::<IncomingFrame>(&text) else {
            continue;
        };

        match frame.kind.as_str() {
            "hello" => {
                let name = frame.name.unwrap_or_else(|| "session".to_string());
                if frame.protocol != Some(PROTOCOL_VERSION) {
                    // Legible mismatch beats a silent half-working room.
                    eprintln!(
                        "[room] \"{name}\" speaks protocol {:?}, room speaks {PROTOCOL_VERSION}",
                        frame.protocol
                    );
                }
                room.add_participant(&name);
                joined_as = Some(name);
                let _ = app.emit("room-participants", room.participants());
            }
            "post" => {
                // Attribution comes from the connection, not from the frame. A
                // sender may name an addressee; it may not name itself.
                let speaker = joined_as.clone().unwrap_or_else(|| "session".to_string());
                deliver(
                    &app,
                    &room,
                    &origin,
                    Post {
                        message_id: frame
                            .message_id
                            .unwrap_or_else(|| Uuid::new_v4().to_string()),
                        speaker,
                        content: frame.content.unwrap_or_default(),
                        to: normalize_to(frame.to),
                        ts: frame.ts.unwrap_or_else(now_iso),
                    },
                );
            }
            _ => {}
        }
    }

    pump.abort();
    if let Some(name) = joined_as {
        room.remove_participant(&name);
        let _ = app.emit("room-participants", room.participants());
    }
    Ok(())
}

// ── Commands ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn room_port(state: tauri::State<RoomState>) -> Option<u16> {
    state.port()
}

#[tauri::command]
pub fn room_participants(state: tauri::State<RoomState>) -> Vec<String> {
    state.participants()
}

/// Seat this screen's person in the room under `name`.
///
/// A person is in the room by being there, not by speaking: without this the
/// roster would list only sessions until the first utterance, and nobody could
/// address someone who had not spoken yet.
#[tauri::command]
pub fn room_join(
    app: AppHandle,
    state: tauri::State<RoomState>,
    name: String,
) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("name is empty".to_string());
    }
    if state.seat_local(&name) {
        let _ = app.emit("room-participants", state.participants());
    }
    Ok(())
}

/// Post this screen's person's utterance into the room.
///
/// Goes through `deliver` like every other post: same frame, same fan-out,
/// same event. The screen does not append locally on send, so the room keeps
/// one ordering authority rather than two.
#[tauri::command]
pub fn room_post(
    app: AppHandle,
    state: tauri::State<RoomState>,
    speaker: String,
    content: String,
    to: Option<String>,
) -> Result<String, String> {
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err("content is empty".to_string());
    }

    let speaker = speaker.trim().to_string();
    if speaker.is_empty() {
        return Err("speaker is empty".to_string());
    }
    // Speaking is being present. A post under a name the roster has not seen
    // seats it, so the two cannot disagree.
    if state.seat_local(&speaker) {
        let _ = app.emit("room-participants", state.participants());
    }

    let message_id = Uuid::new_v4().to_string();
    let local_origin = state.local_origin.clone();
    deliver(
        &app,
        &state,
        &local_origin,
        Post {
            message_id: message_id.clone(),
            speaker,
            content,
            to: normalize_to(to),
            ts: now_iso(),
        },
    );

    Ok(message_id)
}
