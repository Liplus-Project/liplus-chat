// Frame-level round trip for the room sidecar.
//
// Stands a fake room socket up, spawns the sidecar the way a CLI would, and
// drives both faces: MCP over stdio, room frames over WebSocket. This is the
// isolation harness for the round trip — when the real app stops delivering,
// running this says whether the sidecar or the app side moved.
//
// Run: npm run sidecar:test
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { WebSocketServer } from "ws";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const ENTRY = join(HERE, "..", "src", "index.ts");
const REPO = join(HERE, "..", "..");

const TIMEOUT = 20_000;

function deferred() {
  let resolve;
  const promise = new Promise((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

/** Wait for a value, or fail the test with `label` instead of hanging. */
function withTimeout(promise, label) {
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      // unref: a losing race must not hold the event loop open to its deadline.
      setTimeout(() => reject(new Error(`timed out waiting for ${label}`)), TIMEOUT).unref();
    }),
  ]);
}

test("room say reaches the channel, and say_to_room reaches the room", async (t) => {
  // ── fake room ──────────────────────────────────────────────────────────────
  const http = createServer();
  const wss = new WebSocketServer({ server: http });
  await new Promise((r) => http.listen(0, "127.0.0.1", r));
  const port = http.address().port;

  const connected = deferred();
  const helloSeen = deferred();
  const replySeen = deferred();
  let roomSocket = null;

  wss.on("connection", (socket) => {
    roomSocket = socket;
    connected.resolve(socket);
    socket.on("message", (raw) => {
      const frame = JSON.parse(raw.toString());
      if (frame.type === "hello") helloSeen.resolve(frame);
      if (frame.type === "reply") replySeen.resolve(frame);
    });
  });

  // ── sidecar, spawned the way the CLI would ─────────────────────────────────
  const child = spawn(
    process.execPath,
    [join(REPO, "node_modules", "tsx", "dist", "cli.mjs"), ENTRY],
    {
      cwd: REPO,
      env: {
        ...process.env,
        LIPLUS_ROOM_URL: `ws://127.0.0.1:${port}`,
        LIPLUS_AGENT_NAME: "test-agent",
        LIPLUS_ROOM_ID: "test-room",
      },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );

  const stderr = [];
  child.stderr.on("data", (b) => stderr.push(b.toString()));

  t.after(() => {
    child.kill();
    wss.close();
    http.close();
  });

  // ── MCP stdio plumbing: one JSON-RPC message per line ──────────────────────
  const pending = new Map();
  const notifications = [];
  const notificationWaiters = [];
  let buffer = "";

  child.stdout.on("data", (chunk) => {
    buffer += chunk.toString();
    let nl;
    while ((nl = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, nl).trim();
      buffer = buffer.slice(nl + 1);
      if (!line) continue;
      const msg = JSON.parse(line);
      if (msg.id !== undefined && pending.has(msg.id)) {
        pending.get(msg.id).resolve(msg);
        pending.delete(msg.id);
      } else if (msg.method) {
        notifications.push(msg);
        for (const w of notificationWaiters.splice(0)) w(msg);
      }
    }
  });

  let nextId = 1;
  function request(method, params) {
    const id = nextId++;
    const d = deferred();
    pending.set(id, d);
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return withTimeout(d.promise, `response to ${method}`);
  }
  function notify(method, params) {
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`);
  }
  function nextNotification(method) {
    const found = notifications.find((n) => n.method === method);
    if (found) return Promise.resolve(found);
    return withTimeout(
      new Promise((resolve) => {
        const waiter = (msg) => {
          if (msg.method === method) resolve(msg);
          else notificationWaiters.push(waiter);
        };
        notificationWaiters.push(waiter);
      }),
      method,
    );
  }

  // ── initialize: the capability and the manners both ride on this ───────────
  const init = await request("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "round-trip-test", version: "0" },
  });

  assert.ok(
    init.result.capabilities.experimental?.["claude/channel"],
    "server must declare the claude/channel experimental capability",
  );
  assert.match(
    init.result.instructions ?? "",
    /say_to_room/,
    "instructions must name the reply tool",
  );

  notify("notifications/initialized", {});

  const tools = await request("tools/list", {});
  assert.deepEqual(
    tools.result.tools.map((tool) => tool.name),
    ["say_to_room"],
    "exactly one reply tool is exposed",
  );

  // ── room -> agent ──────────────────────────────────────────────────────────
  await withTimeout(connected.promise, "sidecar to connect to the room");
  const hello = await withTimeout(helloSeen.promise, "hello frame");
  assert.equal(hello.agent, "test-agent");
  assert.equal(hello.protocol, 1);

  roomSocket.send(
    JSON.stringify({
      type: "say",
      message_id: "m-1",
      user: "Master",
      content: "聞こえる？",
      ts: "2026-08-21T00:00:00.000Z",
    }),
  );

  const pushed = await nextNotification("notifications/claude/channel");
  assert.match(pushed.params.content, /Master: 聞こえる？/);
  assert.equal(pushed.params.meta.chat_id, "test-room");
  assert.equal(pushed.params.meta.message_id, "m-1");
  assert.equal(pushed.params.meta.user, "Master");

  // ── agent -> room ──────────────────────────────────────────────────────────
  const call = await request("tools/call", {
    name: "say_to_room",
    arguments: { content: "聞こえてるわ", to: "Master" },
  });
  assert.ok(!call.result.isError, `tool call failed: ${JSON.stringify(call.result)}`);

  const reply = await withTimeout(replySeen.promise, "reply frame");
  assert.equal(reply.agent, "test-agent");
  assert.equal(reply.content, "聞こえてるわ");
  assert.equal(reply.to, "Master");

  // ── a dropped frame must not read as delivered ─────────────────────────────
  roomSocket.close();
  await new Promise((r) => setTimeout(r, 500));

  const offline = await request("tools/call", {
    name: "say_to_room",
    arguments: { content: "誰か聞いてる？" },
  });
  assert.ok(
    offline.result.isError,
    "a send with no room attached must report failure, not silence",
  );
});
