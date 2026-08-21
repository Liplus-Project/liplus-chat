// The room surface.
//
// Messages arrive on one event (`room-message`) whether a human or an agent
// spoke, so the room has a single ordering authority. A human utterance is not
// appended locally on send — it comes back through the same event the agents'
// replies do. See src-tauri/src/room.rs.
//
// No terminal here on purpose: a CLI's raw output is not a message source
// (docs/0-requirements.md).
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

const roomEl = document.getElementById("room") as HTMLElement;
const rosterEl = document.getElementById("roster") as HTMLElement;
const nameEl = document.getElementById("display-name") as HTMLInputElement;
const tabEl = document.getElementById("tab-select") as HTMLSelectElement;
const startEl = document.getElementById("start-session") as HTMLButtonElement;
const inputEl = document.getElementById("input") as HTMLTextAreaElement;
const sendEl = document.getElementById("send") as HTMLButtonElement;
const statusEl = document.getElementById("status") as HTMLElement;

let tabs: TabConfig[] = [];

function status(text: string, kind: "info" | "error" = "info"): void {
  statusEl.textContent = text;
  statusEl.dataset.kind = kind;
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
  } catch (err) {
    status(`${tab.name} を起動できませんでした: ${err}`, "error");
  } finally {
    startEl.disabled = false;
  }
}

async function main(): Promise<void> {
  nameEl.value = localStorage.getItem(NAME_KEY) ?? "human";
  nameEl.addEventListener("change", () => {
    localStorage.setItem(NAME_KEY, nameEl.value.trim() || "human");
  });

  await listen<RoomMessage>("room-message", (event) => appendMessage(event.payload));
  await listen<string[]>("room-agents", (event) => renderRoster(event.payload));

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
    if (port === null) status("部屋のソケットがまだ待ち受けていません。", "error");
  } catch (err) {
    status(`部屋の状態を取得できませんでした: ${err}`, "error");
  }
}

void main();
