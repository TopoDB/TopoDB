// Unit: renderer. Integration: run the real hook script against a real
// broker backed by the LOCALLY BUILT server (the pinned npm server is
// 0.0.10 and lacks recent_memories — build with `cargo build -p topodb-mcp`
// first; the test skips loudly if the binary is absent).
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import net from "node:net";
import { fileURLToPath } from "node:url";
import { spawn, execFileSync } from "node:child_process";
import { renderInjection } from "../hooks/session-start.js";
import { connectForProject } from "../broker-client.js";
import { serverArgs } from "../server-args.js";
import { socketPathFor } from "../ipc.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.join(HERE, "..", "..", "..");
const PLUGIN_ROOT = path.join(HERE, "..");
const BROKER_JS = path.join(PLUGIN_ROOT, "broker.js");
const LOCAL_SERVER = path.join(REPO, "target", "debug", process.platform === "win32" ? "topodb-mcp.exe" : "topodb-mcp");

test("renderInjection: caps, formats, and returns null when empty", () => {
  assert.equal(renderInjection([]), null);
  const mems = [
    { id: "01A", content: "Decided to use HNSW behind a flag", entities: ["TopoDB"], ageMs: 86400000, accessCount: 3 },
  ];
  const out = renderInjection(mems);
  assert.match(out, /## TopoDB memory/);
  assert.match(out, /HNSW behind a flag/);
  assert.match(out, /TopoDB/);
  assert.match(out, /search_memories/);
  // Cap: 60 long memories must not exceed ~6000 chars.
  const many = Array.from({ length: 60 }, (_, i) => ({
    id: `01${i}`, content: "x".repeat(300), entities: [], ageMs: 1000, accessCount: 0,
  }));
  assert.ok(renderInjection(many).length <= 6200);
});

// --- integration fixture plumbing ---------------------------------------
//
// The pinned npm server (0.0.10, see server-args.js) does not have
// recent_memories yet — it lands in the same release this plugin ships in,
// but this repo's own devDependency is still on 0.0.10. So this test cannot
// go through launch.js's resolveServer (which insists on the pinned
// version's REAL npm package). Instead it mirrors test/broker.test.js's
// mkFakeCancelServerDataDir technique: a fake `@topodb/topodb-mcp` shim
// package, placed exactly where broker.js's require.resolve looks
// (dataDir/node_modules/@topodb/topodb-mcp/bin/topodb-mcp.js), whose "bin"
// script simply execs the LOCALLY BUILT topodb-mcp binary (which DOES have
// recent_memories) with the argv broker.js hands it. broker.js itself never
// looks at the shim's version — only launch.js's resolveServer does, and
// this test bypasses launch.js entirely, spawning broker.js directly (the
// same thing test/broker.test.js's C2 cancellation test does at line ~894).
//
// This is the PREFERRED variant from the task brief: seed real memories
// through a raw client (here, connectForProject itself, already covered by
// Task 2's own tests) and assert the hook's stdout actually contains both
// seeded contents, exercising recent_memories end to end.
function mkLocalServerDataDir(prefix) {
  const dir = mkdtempSync(path.join(tmpdir(), prefix));
  const pkgDir = path.join(dir, "node_modules", "@topodb", "topodb-mcp");
  mkdirSync(path.join(pkgDir, "bin"), { recursive: true });
  writeFileSync(path.join(pkgDir, "package.json"), JSON.stringify({ name: "@topodb/topodb-mcp", version: "0.0.10", type: "module" }));
  writeFileSync(
    path.join(pkgDir, "bin", "topodb-mcp.js"),
    [
      "import { spawn } from 'node:child_process';",
      "const bin = process.env.TOPODB_MCP_LOCAL_BIN;",
      "const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' });",
      "child.on('exit', (code, signal) => process.exit(code ?? (signal ? 1 : 0)));",
      "child.on('error', () => process.exit(1));",
    ].join("\n"),
  );
  return dir;
}

async function connectSocketWithRetry(sock, { retries = 50, intervalMs = 100 } = {}) {
  for (let i = 0; i < retries; i++) {
    const ok = await new Promise((res) => {
      const c = net.connect(sock);
      c.on("connect", () => {
        c.destroy();
        res(true);
      });
      c.on("error", () => res(false));
    });
    if (ok) return;
    await new Promise((r) => setTimeout(r, intervalMs));
  }
  throw new Error(`broker socket ${sock} never came up`);
}

test(
  "session-start hook injects recent memories end to end",
  { skip: !existsSync(LOCAL_SERVER) && "build topodb-mcp first: cargo build -p topodb-mcp" },
  async () => {
    const dataDir = mkLocalServerDataDir("topodb-ss-");
    const projectDir = mkdtempSync(path.join(tmpdir(), "topodb-ssp-"));
    const args = serverArgs({ projectDir, dataDir });
    const dbPath = args[args.indexOf("--db") + 1];
    const sock = socketPathFor(dbPath);
    let broker = null;
    try {
      broker = spawn(process.execPath, [BROKER_JS, ...args], {
        stdio: ["ignore", "ignore", "pipe"],
        env: { ...process.env, TOPODB_BROKER_IDLE_MS: "5000", TOPODB_MCP_LOCAL_BIN: LOCAL_SERVER },
      });
      let brokerErr = "";
      broker.stderr.on("data", (d) => (brokerErr += d));

      await connectSocketWithRetry(sock);

      // Seed two memories through a real broker session (Task 2's own
      // connectForProject, already unit-tested independently).
      const seeder = await connectForProject({ projectDir, dataDir });
      assert.ok(seeder, `failed to connect to the seeded broker; stderr: ${brokerErr}`);
      await seeder.call("remember", { content: "Decided to use HNSW behind a flag", entities: ["TopoDB"] });
      await seeder.call("remember", { content: "Picked redb over sled for storage", entities: ["TopoDB"] });
      seeder.close();

      const stdinPayload = JSON.stringify({
        session_id: "s1", cwd: projectDir, hook_event_name: "SessionStart", source: "startup",
      });
      const out = execFileSync(process.execPath, [path.join(HERE, "..", "hooks", "session-start.js")], {
        input: stdinPayload,
        env: { ...process.env, CLAUDE_PLUGIN_DATA: dataDir, CLAUDE_PROJECT_DIR: projectDir },
        timeout: 10000,
      }).toString();

      assert.notEqual(out, "", `hook printed nothing; broker stderr: ${brokerErr}`);
      const parsed = JSON.parse(out);
      const ctx = parsed.hookSpecificOutput.additionalContext;
      assert.match(ctx, /HNSW behind a flag/);
      assert.match(ctx, /redb over sled/);
      assert.match(ctx, /TopoDB/);
    } finally {
      if (broker) broker.kill();
      rmSync(dataDir, { recursive: true, force: true });
      rmSync(projectDir, { recursive: true, force: true });
    }
  },
);

test("session-start hook degrades silently with no broker running", async () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-ss3-"));
  const projectDir = mkdtempSync(path.join(tmpdir(), "topodb-ss3p-"));
  const stdinPayload = JSON.stringify({
    session_id: "s1", cwd: projectDir, hook_event_name: "SessionStart", source: "startup",
  });
  try {
    // No broker at all: empty stdout, exit 0 (execFileSync throws on nonzero).
    const out = execFileSync(process.execPath, [path.join(HERE, "..", "hooks", "session-start.js")], {
      input: stdinPayload,
      env: { ...process.env, CLAUDE_PLUGIN_DATA: dataDir, CLAUDE_PROJECT_DIR: projectDir },
      timeout: 10000,
    });
    assert.equal(out.toString(), "");
  } finally {
    rmSync(dataDir, { recursive: true, force: true });
    rmSync(projectDir, { recursive: true, force: true });
  }
});

test("session-start skips subagent sessions and resume/compact", async () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-ss2-"));
  const projectDir = mkdtempSync(path.join(tmpdir(), "topodb-ss2p-"));
  const script = path.join(HERE, "..", "hooks", "session-start.js");
  const cases = [
    { session_id: "s", cwd: projectDir, hook_event_name: "SessionStart", source: "startup", agent_type: "Explore" },
    { session_id: "s", cwd: projectDir, hook_event_name: "SessionStart", source: "resume" },
    { session_id: "s", cwd: projectDir, hook_event_name: "SessionStart", source: "compact" },
  ];
  try {
    for (const payload of cases) {
      const { execFileSync } = await import("node:child_process");
      const out = execFileSync(process.execPath, [script], {
        input: JSON.stringify(payload),
        env: { ...process.env, CLAUDE_PLUGIN_DATA: dataDir, CLAUDE_PROJECT_DIR: projectDir },
        timeout: 5000,
      });
      assert.equal(out.toString(), "", `must stay silent for ${JSON.stringify(payload)}`);
    }
  } finally {
    rmSync(dataDir, { recursive: true, force: true });
    rmSync(projectDir, { recursive: true, force: true });
  }
});
