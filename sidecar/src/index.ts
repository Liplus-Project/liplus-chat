#!/usr/bin/env node
/**
 * liplus-chat room sidecar
 *
 * A stdio MCP server that puts one CLI session into the room.
 *
 * Two protocol faces, mirroring the shape verified in
 * Liplus-Project/github-webhook-mcp (local-mcp/src/index.ts):
 *
 *   CLI  -> sidecar : stdio MCP server. Declares the `claude/channel`
 *                     experimental capability, so the host accepts
 *                     `notifications/claude/channel` pushes from this side.
 *   sidecar -> room : WebSocket client. The app hosts the room socket; the
 *                     CLI spawns this process, so the app cannot know the
 *                     port or the launch moment from its own side.
 *
 * Direction of travel:
 *   room says     -> WebSocket frame -> channel notification -> agent reacts
 *   agent replies -> `say_to_room` tool -> WebSocket frame -> the room
 *
 * The CLI terminal output is never read as a message source. stdout belongs to
 * the MCP transport; every log line goes to stderr.
 */
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  ListToolsRequestSchema,
  CallToolRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import WebSocket from "ws";
import { randomUUID } from "node:crypto";

const ROOM_URL = process.env.LIPLUS_ROOM_URL ?? "";
const AGENT_NAME = process.env.LIPLUS_AGENT_NAME ?? "agent";
const ROOM_TOKEN = process.env.LIPLUS_ROOM_TOKEN ?? "";
const CHAT_ID = process.env.LIPLUS_ROOM_ID ?? "liplus-chat";

const PROTOCOL_VERSION = 1;

function log(line: string): void {
  process.stderr.write(`[liplus-chat sidecar] ${line}\n`);
}

// ── Room frames ──────────────────────────────────────────────────────────────
//
// Room -> sidecar:
//   { type: "say",   message_id, user, content, to?, ts }
// Sidecar -> room:
//   { type: "hello", protocol, agent }
//   { type: "reply", message_id, agent, content, to?, ts }
//
// `to` is optional in both directions and means the same thing on each: the
// display name of the participant addressed. The room fans every frame out to
// everyone regardless — whether an utterance is yours to answer is decided
// here, by the agent, not by the room narrowing its delivery.
//
// Frames whose `type` is unknown are ignored rather than rejected, so the room
// can add frame kinds without breaking a sidecar built against this revision.

interface SayFrame {
  type: "say";
  message_id?: string;
  user?: string;
  content?: string;
  to?: string;
  ts?: string;
}

// ── MCP server ───────────────────────────────────────────────────────────────

const INSTRUCTIONS = [
  "あなたは liplus-chat の部屋に参加しています。",
  `この部屋でのあなたの名前は「${AGENT_NAME}」です。`,
  "",
  '部屋の発言は <channel source="liplus-chat" ...> として届きます。',
  "返信するときは say_to_room ツールを呼んでください。ターミナルへの出力は",
  "部屋には届きません。",
  "",
  "宛先:",
  "- 発言には宛先が付くことがあります。宛先は meta.to に入っています。",
  `- meta.to が「${AGENT_NAME}」なら、あなた宛です。答えてください。`,
  "- meta.to が他の参加者の名前なら、あなた宛ではありません。黙ってください。",
  "  補足したくなっても割り込まないでください。",
  "- meta.to が無い発言は部屋全体宛です。自分が答えるべきときだけ答えてください。",
  "- say_to_room の to 引数で、こちらからも宛先を指定できます。",
  "",
  "部屋の作法:",
  "- 返信しない判断は正当です。全員が答えると部屋は読めなくなります。",
  "- 一度の発言は簡潔に。長い説明が必要なときは、まず要点だけ返してください。",
  "- 他の参加者の発言を、自分の文脈として取り込まないでください。それぞれが",
  "  自分の文脈から同じ会話に参加しています。",
].join("\n");

const mcp = new Server(
  { name: "liplus-chat-room", version: "0.1.0" },
  {
    capabilities: {
      tools: {},
      experimental: { "claude/channel": {} },
    },
    instructions: INSTRUCTIONS,
  },
);

const TOOLS = [
  {
    name: "say_to_room",
    description:
      "Post a message to the liplus-chat room. This is the only way to be heard " +
      "by the room; terminal output is not read by anyone.",
    inputSchema: {
      type: "object" as const,
      properties: {
        content: {
          type: "string",
          description: "The message body to post.",
        },
        to: {
          type: "string",
          description:
            "Optional. The participant this message is addressed to. Omit to " +
            "address the room.",
        },
      },
      required: ["content"],
    },
  },
];

mcp.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: TOOLS }));

mcp.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  if (name !== "say_to_room") {
    return {
      content: [{ type: "text", text: `Unknown tool: ${name}` }],
      isError: true,
    };
  }

  const content = typeof args?.content === "string" ? args.content : "";
  if (!content.trim()) {
    return {
      content: [{ type: "text", text: "content is required and must be non-empty." }],
      isError: true,
    };
  }

  const to = typeof args?.to === "string" ? args.to : undefined;
  const sent = sendToRoom({
    type: "reply",
    message_id: randomUUID(),
    agent: AGENT_NAME,
    content,
    ...(to ? { to } : {}),
    ts: new Date().toISOString(),
  });

  if (!sent) {
    // The room is the only audience. Reporting success on a dropped frame
    // would let the agent believe it had spoken.
    return {
      content: [
        {
          type: "text",
          text: `Not delivered: the room socket is not connected (${roomStatus()}).`,
        },
      ],
      isError: true,
    };
  }

  return { content: [{ type: "text", text: "Delivered to the room." }] };
});

// ── Room socket ──────────────────────────────────────────────────────────────

let ws: WebSocket | null = null;
let pingTimer: ReturnType<typeof setInterval> | null = null;
let retryCount = 0;
let lastError = "";

const BASE_RETRY_DELAY = 1_000;
const MAX_RETRY_DELAY = 30_000;
const PING_INTERVAL = 25_000;

function roomStatus(): string {
  if (!ROOM_URL) return "LIPLUS_ROOM_URL is not set";
  if (ws && ws.readyState === WebSocket.OPEN) return "connected";
  return lastError ? `disconnected: ${lastError}` : "disconnected";
}

function sendToRoom(frame: Record<string, unknown>): boolean {
  if (!ws || ws.readyState !== WebSocket.OPEN) return false;
  try {
    ws.send(JSON.stringify(frame));
    return true;
  } catch (err) {
    lastError = String(err);
    return false;
  }
}

function pushToChannel(frame: SayFrame): void {
  const content = frame.content ?? "";
  if (!content) return;

  const user = frame.user ?? "someone";
  // The addressee rides in meta for the same reason the speaker does: the body
  // must stay equal to what was said. It is judgment material, not text — the
  // instructions tell the agent to read it and decide whether to answer.
  const to = typeof frame.to === "string" && frame.to ? frame.to : undefined;
  void mcp.notification({
    method: "notifications/claude/channel",
    params: {
      // Body only. The speaker rides in meta, which the host renders itself —
      // putting the name here too produced "マスター: マスター: ハロ～" (#28),
      // and it leaves the body no longer equal to what was said.
      content,
      meta: {
        chat_id: CHAT_ID,
        message_id: frame.message_id ?? randomUUID(),
        user,
        ...(to ? { to } : {}),
        ts: frame.ts ?? new Date().toISOString(),
      },
    },
  });
}

function scheduleRetry(): void {
  const delay = Math.min(BASE_RETRY_DELAY * 2 ** retryCount, MAX_RETRY_DELAY);
  retryCount++;
  log(`room socket: retrying in ${Math.round(delay / 1000)}s (attempt ${retryCount})`);
  setTimeout(connectRoom, delay);
}

function connectRoom(): void {
  const headers: Record<string, string> = {};
  if (ROOM_TOKEN) headers["Authorization"] = `Bearer ${ROOM_TOKEN}`;

  const socket = new WebSocket(ROOM_URL, { headers });
  ws = socket;

  socket.on("open", () => {
    retryCount = 0;
    lastError = "";
    log(`room socket: connected as "${AGENT_NAME}"`);
    sendToRoom({ type: "hello", protocol: PROTOCOL_VERSION, agent: AGENT_NAME });
    pingTimer = setInterval(() => {
      if (socket.readyState === WebSocket.OPEN) socket.ping();
    }, PING_INTERVAL);
  });

  socket.on("message", (raw: Buffer | string) => {
    let data: unknown;
    try {
      data = JSON.parse(raw.toString());
    } catch {
      return;
    }
    if (typeof data !== "object" || data === null) return;
    const frame = data as { type?: string };
    if (frame.type === "say") pushToChannel(frame as SayFrame);
    // Unknown frame kinds are ignored on purpose; see the frame comment above.
  });

  socket.on("close", (code: number) => {
    if (pingTimer) clearInterval(pingTimer);
    pingTimer = null;
    ws = null;
    lastError = `closed with code ${code}`;
    log(`room socket: ${lastError}`);
    scheduleRetry();
  });

  socket.on("error", (err: Error) => {
    // A failed connect emits error then close; the close handler reconnects.
    lastError = err.message;
    log(`room socket: error: ${err.message}`);
  });
}

// ── Main ─────────────────────────────────────────────────────────────────────

await mcp.connect(new StdioServerTransport());
log("mcp: stdio transport connected");

if (ROOM_URL) {
  connectRoom();
} else {
  // Serving MCP without a room is a degraded but legible state: the agent can
  // still call the tool and gets told why nothing was delivered. Exiting here
  // would surface to the user as a bare MCP connection failure instead.
  log("room socket: LIPLUS_ROOM_URL is not set, staying offline");
}
