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

test("a room post reaches the channel, and say_to_room reaches the room", async (t) => {
  // ── fake room ──────────────────────────────────────────────────────────────
  const http = createServer();
  const wss = new WebSocketServer({ server: http });
  await new Promise((r) => http.listen(0, "127.0.0.1", r));
  const port = http.address().port;

  const connected = deferred();
  const helloSeen = deferred();
  const postSeen = deferred();
  let roomSocket = null;

  wss.on("connection", (socket) => {
    roomSocket = socket;
    connected.resolve(socket);
    socket.on("message", (raw) => {
      const frame = JSON.parse(raw.toString());
      if (frame.type === "hello") helloSeen.resolve(frame);
      if (frame.type === "post") postSeen.resolve(frame);
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
  /** The `index`-th notification of `method`, awaited if it has not arrived. */
  function nextNotification(method, index = 0) {
    const matching = () => notifications.filter((n) => n.method === method);
    if (matching().length > index) return Promise.resolve(matching()[index]);
    return withTimeout(
      new Promise((resolve) => {
        const waiter = () => {
          const seen = matching();
          if (seen.length > index) resolve(seen[index]);
          else notificationWaiters.push(waiter);
        };
        notificationWaiters.push(waiter);
      }),
      `${method} #${index}`,
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
    "instructions must name the posting tool",
  );
  // The manners and the material they are judged on ship together. Manners
  // that say "answer what is addressed to you" without naming where the
  // addressee is, or without naming what this agent is called, ask for a
  // judgment the agent has nothing to make.
  assert.match(
    init.result.instructions ?? "",
    /meta\.to/,
    "instructions must name the addressee as judgment material",
  );
  assert.match(
    init.result.instructions ?? "",
    /test-agent/,
    "instructions must tell the agent the name it answers to",
  );
  // The model lives in the manners as much as in the frames. An agent told to
  // answer "the human" would be reading a distinction the protocol no longer
  // carries (#39).
  assert.match(
    init.result.instructions ?? "",
    /人間と AI を区別しません/,
    "instructions must state that participants are not split into human and AI",
  );

  notify("notifications/initialized", {});

  const tools = await request("tools/list", {});
  assert.deepEqual(
    tools.result.tools.map((tool) => tool.name),
    ["say_to_room"],
    "exactly one posting tool is exposed",
  );

  // ── room -> agent ──────────────────────────────────────────────────────────
  await withTimeout(connected.promise, "sidecar to connect to the room");
  const hello = await withTimeout(helloSeen.promise, "hello frame");
  assert.equal(hello.name, "test-agent");
  assert.equal(hello.protocol, 2);

  roomSocket.send(
    JSON.stringify({
      type: "post",
      message_id: "m-1",
      speaker: "Master",
      content: "聞こえる？",
      ts: "2026-08-21T00:00:00.000Z",
    }),
  );

  const pushed = await nextNotification("notifications/claude/channel");
  // Body only: the speaker belongs in meta, and the host renders it. Mixing it
  // into the body showed the name twice on screen (#28).
  assert.equal(pushed.params.content, "聞こえる？");
  assert.equal(pushed.params.meta.chat_id, "test-room");
  assert.equal(pushed.params.meta.message_id, "m-1");
  assert.equal(pushed.params.meta.user, "Master");
  // An unaddressed utterance is the room as a whole. No key, rather than an
  // empty one: an agent testing `meta.to` must not read "" as a name.
  assert.equal(
    "to" in pushed.params.meta,
    false,
    "an utterance with no addressee must carry no `to`",
  );

  // ── the addressee rides through to the agent ───────────────────────────────
  roomSocket.send(
    JSON.stringify({
      type: "post",
      message_id: "m-2",
      speaker: "Master",
      content: "リンだけ答えて",
      to: "test-agent",
      ts: "2026-08-21T00:00:01.000Z",
    }),
  );

  const addressed = await nextNotification("notifications/claude/channel", 1);
  // In meta, next to the speaker, for the same reason: the body stays equal to
  // what was said.
  assert.equal(addressed.params.content, "リンだけ答えて");
  assert.equal(addressed.params.meta.to, "test-agent");

  // Addressed elsewhere still arrives — the room delivers to everyone and the
  // agent decides. Filtering here would put "who heard it" in the room.
  roomSocket.send(
    JSON.stringify({
      type: "post",
      message_id: "m-3",
      speaker: "Master",
      content: "レイはどう思う",
      to: "other-agent",
      ts: "2026-08-21T00:00:02.000Z",
    }),
  );

  const elsewhere = await nextNotification("notifications/claude/channel", 2);
  assert.equal(elsewhere.params.meta.to, "other-agent");

  // ── this participant -> room ───────────────────────────────────────────────
  const call = await request("tools/call", {
    name: "say_to_room",
    arguments: { content: "聞こえてるわ", to: "Master" },
  });
  assert.ok(!call.result.isError, `tool call failed: ${JSON.stringify(call.result)}`);

  const post = await withTimeout(postSeen.promise, "post frame");
  assert.equal(post.type, "post");
  assert.equal(post.content, "聞こえてるわ");
  // A person is addressed exactly like a session. One vocabulary, one frame.
  assert.equal(post.to, "Master");
  // Attribution belongs to the room, stamped from the connection. A sender
  // that could name itself could name somebody else.
  assert.equal(
    "speaker" in post,
    false,
    "a posting participant must not name itself; the room stamps the speaker",
  );

  // ── suppression is the room's, and is not decided by name ─────────────────
  // The room drops a post on the connection that produced it, so this side
  // never has to recognise itself. It must not second-guess that with a name
  // test: while two participants share a name, a name test here would swallow
  // the other one's posts too (#40).
  roomSocket.send(
    JSON.stringify({
      type: "post",
      message_id: "m-4",
      speaker: "test-agent",
      content: "同じ名前の別参加者",
      ts: "2026-08-21T00:00:03.000Z",
    }),
  );

  const sameName = await nextNotification("notifications/claude/channel", 3);
  assert.equal(sameName.params.meta.message_id, "m-4");
  assert.equal(sameName.params.meta.user, "test-agent");

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
