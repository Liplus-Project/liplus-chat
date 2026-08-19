// Entry point placeholder.
//
// The chat room UI is not implemented yet: the room surface (message list,
// speaker attribution, composer) lands with the MCP channel wiring, not with
// this scaffold. See issue #1 for what this commit does and does not cover.

const room = document.getElementById("room");
if (room) {
  room.textContent = "Li+ Chat — scaffold only. The room is not wired yet.";
}
