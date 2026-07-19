// Integration: connectForProject against a REAL broker. Spawns launch.js
// (which starts a broker against the pinned server), then connects the
// hook client to the same socket and round-trips real tool calls.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import net from "node:net";
import { connectForProject } from "../broker-client.js";
import { socketPathFor } from "../ipc.js";
import { serverArgs } from "../server-args.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const LAUNCH_JS = path.join(HERE, "..", "launch.js");

test("hook client connects, handshakes, and round-trips tool calls", async () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-bc-"));
  const projectDir = mkdtempSync(path.join(tmpdir(), "topodb-proj-"));
  // A real shim starts the broker; we ride its socket like a hook would.
  const shim = spawn(process.execPath, [LAUNCH_JS], {
    env: {
      ...process.env,
      CLAUDE_PLUGIN_DATA: dataDir,
      CLAUDE_PROJECT_DIR: projectDir,
      TOPODB_BROKER_IDLE_MS: "5000",
    },
    stdio: ["pipe", "pipe", "pipe"],
  });
  try {
    // Give the shim time to spawn the broker and bind the socket.
    let client = null;
    for (let i = 0; i < 50 && !client; i++) {
      await new Promise((r) => setTimeout(r, 200));
      client = await connectForProject({ projectDir, dataDir });
    }
    assert.ok(client, "hook client must reach the broker the shim started");
    try {
      const created = await client.call("create_memory", {
        content: "broker-client round trip",
      });
      assert.ok(typeof created.id === "string", `create result: ${JSON.stringify(created)}`);
      const node = await client.call("get_node", { id: created.id });
      assert.equal(node.found, true);
      assert.equal(node.node.props.content, "broker-client round trip");
    } finally {
      client.close();
    }
  } finally {
    shim.kill();
    rmSync(dataDir, { recursive: true, force: true });
    rmSync(projectDir, { recursive: true, force: true });
  }
});

test("absent broker resolves null, never throws", async () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-bc-none-"));
  const projectDir = mkdtempSync(path.join(tmpdir(), "topodb-projn-"));
  try {
    const client = await connectForProject({ projectDir, dataDir, connectTimeoutMs: 300 });
    assert.equal(client, null);
  } finally {
    rmSync(dataDir, { recursive: true, force: true });
    rmSync(projectDir, { recursive: true, force: true });
  }
});

test("a connection that accepts but never speaks the broker's protocol resolves null, never throws", async () => {
  // Regression: a socket file can outlive its broker (crash, stale mount,
  // some unrelated process squatting on the path). The TCP/unix-socket
  // CONNECT succeeds in that case -- only the handshake that follows fails --
  // so this must be handled distinctly from "absent broker" above (which
  // fails at connect time). A bare net.createServer() that accepts and never
  // replies stands in for exactly that: connect succeeds, initialize hangs.
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-bc-mute-"));
  const projectDir = mkdtempSync(path.join(tmpdir(), "topodb-projm-"));
  const args = serverArgs({ projectDir, dataDir });
  const dbPath = args[args.indexOf("--db") + 1];
  const sock = socketPathFor(dbPath);
  const fakeServer = net.createServer((socket) => {
    socket.on("data", () => {}); // accept bytes, never answer
  });
  try {
    await new Promise((resolve, reject) => {
      fakeServer.once("error", reject);
      fakeServer.listen(sock, resolve);
    });

    const started = Date.now();
    const client = await connectForProject({ projectDir, dataDir, connectTimeoutMs: 300 });
    const elapsedMs = Date.now() - started;

    assert.equal(client, null, "a mute peer on the socket must degrade to null, not throw or hang");
    assert.ok(
      elapsedMs < 3000,
      `expected connectForProject to give up within ~3s (bounded by initialize's own 2000ms rpc timeout), took ${elapsedMs}ms`,
    );
  } finally {
    await new Promise((resolve) => fakeServer.close(resolve));
    rmSync(dataDir, { recursive: true, force: true });
    rmSync(projectDir, { recursive: true, force: true });
  }
});
