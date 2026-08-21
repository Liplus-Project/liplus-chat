// The room surface.
//
// Messages arrive on one event (`room-message`) whether a human or an agent
// spoke, so the room has a single ordering authority. A human utterance is not
// appended locally on send — it comes back through the same event the agents'
// replies do. See src-tauri/src/room.rs.
//
// The diagnostics pane carries a real terminal for the launched CLI. That is a
// display, not a message source: the room's lines come from the channel and
// from `say_to_room`, and nothing in this file reads terminal output as
// speech. The rejected design is the one where the app parses CLI output to
// find messages (docs/0-requirements.md); showing the CLI is not that.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { readText } from "@tauri-apps/plugin-clipboard-manager";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

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
const diagnosticsEl = document.getElementById("diagnostics") as HTMLElement;
const toggleEl = document.getElementById("toggle-diagnostics") as HTMLButtonElement;
const socketStateEl = document.getElementById("socket-state") as HTMLElement;
const sessionStateEl = document.getElementById("session-state") as HTMLElement;
const terminalEl = document.getElementById("terminal") as HTMLElement;
const cwdEl = document.getElementById("session-cwd") as HTMLInputElement;

let tabs: TabConfig[] = [];
/** The session the terminal is attached to, once one is running. */
let activePtyId: string | null = null;

const terminal = new Terminal({
  cursorBlink: true,
  fontSize: 13,
  fontFamily: 'ui-monospace, "Cascadia Mono", Consolas, monospace',
  // The CLI is a full-screen TUI: it moves the cursor, clears regions and
  // repaints. Anything less than an emulator turns that into debris, which is
  // what the previous line-appending pane did (#24).
  convertEol: false,
  scrollback: 5000,
});
const fitAddon = new FitAddon();

function status(text: string, kind: "info" | "error" = "info"): void {
  statusEl.textContent = text;
  statusEl.dataset.kind = kind;
}

function revealDiagnostics(): void {
  diagnosticsEl.hidden = false;
  toggleEl.setAttribute("aria-expanded", "true");
  // The container has no size while hidden, so the fit has to wait for layout.
  requestAnimationFrame(() => fitTerminal());
}

function fitTerminal(): void {
  if (diagnosticsEl.hidden) return;
  try {
    fitAddon.fit();
  } catch {
    // A fit against a zero-sized container is not worth a message.
    return;
  }
  if (activePtyId !== null) {
    void invoke("resize_pty", {
      id: activePtyId,
      cols: terminal.cols,
      rows: terminal.rows,
    }).catch(() => {
      // The session may have exited between the fit and the call.
    });
  }
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
 * the room simply stays empty. Without this the screen is identical whether
 * the CLI is running or was never there.
 */
async function followSession(tab: TabConfig, started: StartedSession): Promise<void> {
  sessionStateEl.textContent = `${tab.name} 起動中`;
  sessionStateEl.dataset.kind = "ok";
  activePtyId = started.pty_id;

  await listen<string>(`pty-data-${started.pty_id}`, (event) => terminal.write(event.payload));
  await listen<number | null>(`pty-exit-${started.pty_id}`, (event) => {
    const code = event.payload;
    const detail = code === null ? "終了コード不明" : `終了コード ${code}`;
    sessionStateEl.textContent = `${tab.name} 終了（${detail}）`;
    sessionStateEl.dataset.kind = "error";
    status(`${tab.name} が終了しました（${detail}）。診断を確認してください。`, "error");
    activePtyId = null;
    revealDiagnostics();
  });

  // The first thing a session shows is a question, so the pane that carries
  // the answer opens with it rather than waiting for a failure.
  revealDiagnostics();
  terminal.focus();
}

async function startSession(): Promise<void> {
  const tab = tabs.find((candidate) => candidate.id === tabEl.value);
  if (!tab) {
    status("起動するセッションが選ばれていません。", "error");
    return;
  }

  const cwd = cwdEl.value.trim();
  if (!cwd) {
    status("作業ディレクトリを入力してください。", "error");
    cwdEl.focus();
    return;
  }
  // The directory is the person's choice, so it is carried on the tab and
  // saved. Falling back to whatever directory the app process happens to sit
  // in is what put a session in src-tauri (#20).
  const launching: TabConfig = { ...tab, cwd };

  // Size the PTY to the terminal that will display it, so the CLI's first
  // paint is not laid out for a window it does not have.
  revealDiagnostics();
  fitAddon.fit();

  startEl.disabled = true;
  status(`${tab.name} を起動しています…`);
  try {
    const started = await invoke<StartedSession>("start_session", {
      tab: launching,
      cols: terminal.cols,
      rows: terminal.rows,
    });
    tab.cwd = cwd;
    void invoke("save_config", { config: { tabs } }).catch(() => {
      // A directory that fails to persist is worth one line, not a failed
      // launch: the session is already up.
      status("作業ディレクトリを保存できませんでした。", "error");
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

function setUpTerminal(): void {
  terminal.loadAddon(fitAddon);
  terminal.open(terminalEl);

  const style = getComputedStyle(document.documentElement);
  terminal.options.theme = {
    background: style.getPropertyValue("--bg").trim() || "#17171a",
    foreground: style.getPropertyValue("--fg").trim() || "#e8e8ea",
  };

  terminal.onData((data) => {
    if (activePtyId === null) return;
    void invoke("write_pty", { id: activePtyId, data }).catch((err) => {
      status(`セッションへ送れませんでした: ${err}`, "error");
    });
  });

  // The webview does not deliver a native paste to xterm, so Ctrl+V is bridged
  // explicitly. preventDefault stops the input arriving twice.
  terminal.attachCustomKeyEventHandler((event) => {
    const isPaste =
      event.type === "keydown" &&
      (event.ctrlKey || event.metaKey) &&
      !event.altKey &&
      (event.key === "v" || event.key === "V");
    if (!isPaste) return true;

    event.preventDefault();
    void readText().then((text) => {
      if (text && activePtyId !== null) void invoke("write_pty", { id: activePtyId, data: text });
    });
    return false;
  });

  new ResizeObserver(() => fitTerminal()).observe(terminalEl);
}

async function main(): Promise<void> {
  setUpTerminal();

  nameEl.value = localStorage.getItem(NAME_KEY) ?? "human";
  nameEl.addEventListener("change", () => {
    localStorage.setItem(NAME_KEY, nameEl.value.trim() || "human");
  });

  toggleEl.addEventListener("click", () => {
    if (diagnosticsEl.hidden) {
      revealDiagnostics();
    } else {
      diagnosticsEl.hidden = true;
      toggleEl.setAttribute("aria-expanded", "false");
    }
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

  let home = "";
  try {
    home = await invoke<string>("home_dir");
  } catch {
    // Only the prefill is lost; the field is still typed into by hand.
  }

  const showCwd = (): void => {
    const tab = tabs.find((candidate) => candidate.id === tabEl.value);
    cwdEl.value = tab?.cwd ?? home;
  };

  try {
    const config = await invoke<AppConfig>("load_config");
    tabs = config.tabs;
    for (const tab of tabs) {
      const option = document.createElement("option");
      option.value = tab.id;
      option.textContent = tab.name;
      tabEl.appendChild(option);
    }
    showCwd();
    tabEl.addEventListener("change", showCwd);
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
