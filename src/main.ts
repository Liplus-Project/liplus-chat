// The room surface.
//
// Messages arrive on one event (`room-message`) whether a human or an agent
// spoke, so the room has a single ordering authority. A human utterance is not
// appended locally on send — it comes back through the same event the agents'
// replies do. See src-tauri/src/room.rs.
//
// No terminal in the room on purpose: a CLI's raw output is not a message
// source (docs/0-requirements.md). The diagnostics pane is a separate surface
// and exists for the opposite reason — when nothing arrives, the machinery
// under the room has to be readable without a console.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface RoomMessage {
  message_id: string;
  /** "human" or "agent". */
  kind: string;
  speaker: string;
  content: string;
  to: string | null;
  ts: string;
}

interface TabConfig {
  id: string;
  name: string;
  command: string;
  args: string[];
  cwd: string | null;
  cli_kind: string;
}

interface AppConfig {
  tabs: TabConfig[];
}

interface StartedSession {
  pty_id: string;
  mcp_config: string;
}

const NAME_KEY = "liplus-chat.display-name";
/** Lines of CLI output kept for triage. Enough to hold a startup failure. */
const LOG_LIMIT = 400;

const roomEl = document.getElementById("room") as HTMLElement;
const rosterEl = document.getElementById("roster") as HTMLElement;
const nameEl = document.getElementById("display-name") as HTMLInputElement;
const tabEl = document.getElementById("tab-select") as HTMLSelectElement;
const startEl = document.getElementById("start-session") as HTMLButtonElement;
const inputEl = document.getElementById("input") as HTMLTextAreaElement;
const sendEl = document.getElementById("send") as HTMLButtonElement;
const statusEl = document.getElementById("status") as HTMLElement;
const diagnosticsEl = document.getElementById("diagnostics") as HTMLElement;
const toggleEl = document.getElementById("toggle-diagnostics") as HTMLButtonElement;
const socketStateEl = document.getElementById("socket-state") as HTMLElement;
const sessionStateEl = document.getElementById("session-state") as HTMLElement;
const logEl = document.getElementById("session-log") as HTMLPreElement;

let tabs: TabConfig[] = [];
let logLines: string[] = [];

function status(text: string, kind: "info" | "error" = "info"): void {
  statusEl.textContent = text;
  statusEl.dataset.kind = kind;
}

/** Open the diagnostics pane. Called when something goes wrong on its own. */
function revealDiagnostics(): void {
  diagnosticsEl.hidden = false;
  toggleEl.setAttribute("aria-expanded", "true");
}

// Terminal control sequences, built from escapes rather than written as
// literal control bytes: a raw 0x1b in the source survives an editor round
// trip only by luck. This pane is read, not driven, so the sequences are
// stripped rather than interpreted.
const CSI = /\x1b\[[0-9;?]*[ -/]*[@-~]/g;
const OSC = /\x1b[\]P^_][^\x1b\x07]*(?:\x1b\\|\x07)?/g;
const SHORT_ESCAPE = /\x1b[@-Z\\-_]/g;

function appendLog(chunk: string): void {
  const plain = chunk
    .replace(CSI, "")
    .replace(OSC, "")
    .replace(SHORT_ESCAPE, "")
    .replace(/\r/g, "");

  const atBottom = logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight < 40;
  logLines.push(...plain.split("\n"));
  if (logLines.length > LOG_LIMIT) logLines = logLines.slice(-LOG_LIMIT);
  logEl.textContent = logLines.join("\n");
  if (atBottom) logEl.scrollTop = logEl.scrollHeight;
}

function shortTime(iso: string): string {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return "";
  return at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function appendMessage(message: RoomMessage): void {
  // The room is scrolled to the bottom only when it already was, so reading
  // back through the log is not yanked away by an arriving message.
  const atBottom = roomEl.scrollHeight - roomEl.scrollTop - roomEl.clientHeight < 40;

  const line = document.createElement("article");
  line.className = "message";
  line.dataset.kind = message.kind;

  const head = document.createElement("div");
  head.className = "meta";

  const speaker = document.createElement("span");
  speaker.className = "speaker";
  speaker.textContent = message.speaker;
  head.appendChild(speaker);

  if (message.to) {
    const to = document.createElement("span");
    to.className = "to";
    to.textContent = `→ ${message.to}`;
    head.appendChild(to);
  }

  const time = document.createElement("time");
  time.className = "ts";
  time.dateTime = message.ts;
  time.textContent = shortTime(message.ts);
  head.appendChild(time);

  const body = document.createElement("div");
  body.className = "body";
  body.textContent = message.content;

  line.append(head, body);
  roomEl.appendChild(line);

  if (atBottom) roomEl.scrollTop = roomEl.scrollHeight;
}

function renderRoster(agents: string[]): void {
  rosterEl.textContent = agents.length ? agents.join(" / ") : "参加者なし";
}

async function send(): Promise<void> {
  const content = inputEl.value.trim();
  if (!content) return;

  const user = nameEl.value.trim() || "human";
  inputEl.value = "";
  try {
    await invoke<string>("room_say", { user, content });
    status("");
  } catch (err) {
    // Put the text back rather than losing what was typed.
    inputEl.value = content;
    status(`発言を送れませんでした: ${err}`, "error");
  }
}

/**
 * Follow a launched session until it dies.
 *
 * A session that exits on startup is the failure mode with no other witness:
 * there is no terminal, and the room simply stays empty. Without this the
 * screen is identical whether the CLI is running or was never there.
 */
async function followSession(tab: TabConfig, started: StartedSession): Promise<void> {
  sessionStateEl.textContent = `${tab.name} 起動中`;
  sessionStateEl.dataset.kind = "ok";

  await listen<string>(`pty-data-${started.pty_id}`, (event) => appendLog(event.payload));
  await listen<number | null>(`pty-exit-${started.pty_id}`, (event) => {
    const code = event.payload;
    const detail = code === null ? "終了コード不明" : `終了コード ${code}`;
    sessionStateEl.textContent = `${tab.name} 終了（${detail}）`;
    sessionStateEl.dataset.kind = "error";
    status(`${tab.name} が終了しました（${detail}）。診断を確認してください。`, "error");
    revealDiagnostics();
  });
}

async function startSession(): Promise<void> {
  const tab = tabs.find((candidate) => candidate.id === tabEl.value);
  if (!tab) {
    status("起動するセッションが選ばれていません。", "error");
    return;
  }

  startEl.disabled = true;
  status(`${tab.name} を起動しています…`);
  try {
    const started = await invoke<StartedSession>("start_session", {
      tab,
      cols: 120,
      rows: 30,
    });
    status(`${tab.name} を起動しました。${started.mcp_config} に登録済み。`);
    await followSession(tab, started);
  } catch (err) {
    status(`${tab.name} を起動できませんでした: ${err}`, "error");
    sessionStateEl.textContent = `${tab.name} 起動失敗`;
    sessionStateEl.dataset.kind = "error";
    revealDiagnostics();
  } finally {
    startEl.disabled = false;
  }
}

function renderSocket(port: number | null, error?: string): void {
  if (error) {
    socketStateEl.textContent = error;
    socketStateEl.dataset.kind = "error";
    return;
  }
  if (port === null) {
    socketStateEl.textContent = "未待受";
    socketStateEl.dataset.kind = "error";
    return;
  }
  socketStateEl.textContent = `127.0.0.1:${port} で待受中`;
  socketStateEl.dataset.kind = "ok";
}

async function main(): Promise<void> {
  nameEl.value = localStorage.getItem(NAME_KEY) ?? "human";
  nameEl.addEventListener("change", () => {
    localStorage.setItem(NAME_KEY, nameEl.value.trim() || "human");
  });

  toggleEl.addEventListener("click", () => {
    const open = diagnosticsEl.hidden;
    diagnosticsEl.hidden = !open;
    toggleEl.setAttribute("aria-expanded", String(open));
  });

  await listen<RoomMessage>("room-message", (event) => appendMessage(event.payload));
  await listen<string[]>("room-agents", (event) => renderRoster(event.payload));
  // The socket binds after the frontend loads, so the event is the authority
  // and the poll below is only for a listener that attached too late.
  await listen<number>("room-ready", (event) => renderSocket(event.payload));

  sendEl.addEventListener("click", () => void send());
  inputEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      void send();
    }
  });
  startEl.addEventListener("click", () => void startSession());

  try {
    const config = await invoke<AppConfig>("load_config");
    tabs = config.tabs;
    for (const tab of tabs) {
      const option = document.createElement("option");
      option.value = tab.id;
      option.textContent = tab.name;
      tabEl.appendChild(option);
    }
  } catch (err) {
    status(`設定を読み込めませんでした: ${err}`, "error");
  }

  try {
    renderRoster(await invoke<string[]>("room_agents"));
    const port = await invoke<number | null>("room_port");
    if (port !== null) renderSocket(port);
  } catch (err) {
    renderSocket(null, `取得できませんでした: ${err}`);
  }
}

void main();
