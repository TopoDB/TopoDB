// Unit: task extraction, skip logic, context rendering. Integration: run the real
// hook script against a real broker backed by the LOCALLY BUILT server
// (same pattern as session-start.test.js: spawn broker.js with a fake
// @topodb/topodb-mcp shim that execs the local binary).
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, rmSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import net from "node:net";
import { fileURLToPath } from "node:url";
import { extractTask, skipSet, renderSubagentContext } from "../hooks/subagent-start.js";
import { connectForProject } from "../broker-client.js";
import { serverArgs } from "../server-args.js";
import { socketPathFor } from "../ipc.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN_ROOT = path.join(HERE, "..");
const REPO = path.join(PLUGIN_ROOT, "..", "..");
const HOOK = path.join(PLUGIN_ROOT, "hooks", "subagent-start.js");
const BROKER_JS = path.join(PLUGIN_ROOT, "broker.js");
const LOCAL_SERVER = path.join(REPO, "target", "debug", process.platform === "win32" ? "topodb-mcp.exe" : "topodb-mcp");

// --- Unit tests: extractTask ---

test("extractTask: string content", () => {
  const transcript = JSON.stringify({ type: "user", message: { role: "user", content: "find the bug" } }) + "\n";
  assert.equal(extractTask(transcript), "find the bug");
});

test("extractTask: array-of-text-parts content, flattened", () => {
  const transcript = JSON.stringify({
    type: "user",
    message: {
      role: "user",
      content: [{ type: "text", text: "part 1" }, { type: "text", text: " part 2" }],
    },
  }) + "\n";
  assert.equal(extractTask(transcript), "part 1 part 2");
});

test("extractTask: caps at 1000 chars", () => {
  const long = "x".repeat(1500);
  const transcript = JSON.stringify({ type: "user", message: { role: "user", content: long } }) + "\n";
  const result = extractTask(transcript);
  assert.equal(result.length, 1000);
  assert.match(result, /^x{1000}$/);
});

test("extractTask: null when no user message / empty / unparseable / whitespace-only", () => {
  const cases = [
    null,
    "",
    "not json",
    JSON.stringify({ type: "assistant", message: { role: "assistant", content: "x" } }),
    JSON.stringify({ type: "user", message: { role: "user", content: "" } }),
    JSON.stringify({ type: "user", message: { role: "user", content: "   " } }),
  ];
  for (const input of cases) {
    assert.equal(extractTask(input), null, `should return null for: ${input}`);
  }
});

// --- Unit tests: skipSet ---

test("skipSet: default skips explore+plan case-insensitively", () => {
  const set = skipSet({});
  // Set stores lowercase versions
  assert.ok(set.has("explore"));
  assert.ok(set.has("plan"));
  assert.ok(!set.has("general-purpose"));
  // Verify the callers need to lowercase before checking (as main() does)
  assert.ok(set.has(String("EXPLORE").toLowerCase()));
  assert.ok(set.has(String("PLAN").toLowerCase()));
});

test("skipSet: env TOPODB_SUBAGENT_SKIP replaces default", () => {
  const set = skipSet({ TOPODB_SUBAGENT_SKIP: "foo,bar" });
  assert.ok(set.has("foo"));
  assert.ok(set.has("bar"));
  assert.ok(!set.has("explore"));
});

test("skipSet: empty string → empty set", () => {
  const set = skipSet({ TOPODB_SUBAGENT_SKIP: "" });
  assert.equal(set.size, 0);
});

// --- Unit tests: renderSubagentContext ---

test("renderSubagentContext: empty array → null", () => {
  assert.equal(renderSubagentContext([]), null);
});

test("renderSubagentContext: non-Memory label filtered out", () => {
  const nodes = [{ label: "Entity", props: { content: "entity content" } }];
  assert.equal(renderSubagentContext(nodes), null);
});

test("renderSubagentContext: Memory with missing/whitespace content filtered out", () => {
  const nodes = [
    { label: "Memory", props: {} },
    { label: "Memory", props: { content: "" } },
    { label: "Memory", props: { content: "   " } },
  ];
  assert.equal(renderSubagentContext(nodes), null);
});

test("renderSubagentContext: all nodes filter out → null", () => {
  const nodes = [
    { label: "Entity", props: { content: "x" } },
    { label: "Memory", props: { content: "" } },
  ];
  assert.equal(renderSubagentContext(nodes), null);
});

test("renderSubagentContext: one or more Memory nodes with content → injects header, content, trailer", () => {
  const nodes = [
    { label: "Memory", props: { content: "First memory fact" } },
    { label: "Entity", props: { name: "SomeThing" } },
    { label: "Memory", props: { content: "Second memory fact" } },
  ];
  const out = renderSubagentContext(nodes);
  assert.ok(out);
  assert.match(out, /## Relevant project memory/);
  assert.match(out, /First memory fact/);
  assert.match(out, /Second memory fact/);
  assert.match(out, /Recall more: search_memories\. Store: remember\./);
});

// --- Run the hook with a payload + env via execFileSync, return trimmed stdout.
function runHook(payload, extraEnv = {}) {
  try {
    return execFileSync(process.execPath, [HOOK], {
      input: JSON.stringify(payload),
      env: { ...process.env, ...extraEnv },
      timeout: 10000,
    }).toString().trim();
  } catch {
    return "";
  }
}

function tmpTranscript(content) {
  const dir = mkdtempSync(path.join(tmpdir(), "sub-"));
  const t = path.join(dir, "t.jsonl");
  writeFileSync(t, JSON.stringify({ type: "user", message: { role: "user", content } }) + "\n");
  return { dir, t };
}

// --- Control-flow tests (no server needed) ---

test("main: not a subagent event (no agent_id) → no stdout", async () => {
  const { dir, t } = tmpTranscript("anything");
  assert.equal(await runHook({ agent_type: "general-purpose", cwd: dir, transcript_path: t }, { CLAUDE_PLUGIN_DATA: dir, CLAUDE_PROJECT_DIR: dir }), "");
});

test("main: Explore agent_type is skipped → no stdout", async () => {
  const { dir, t } = tmpTranscript("anything");
  assert.equal(await runHook({ agent_id: "a", agent_type: "Explore", cwd: dir, transcript_path: t }, { CLAUDE_PLUGIN_DATA: dir, CLAUDE_PROJECT_DIR: dir }), "");
});

test("main: no extractable task → no stdout (returns before broker)", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "sub-"));
  const t = path.join(dir, "t.jsonl");
  writeFileSync(t, JSON.stringify({ type: "assistant", message: { role: "assistant", content: "no user turn" } }) + "\n");
  assert.equal(await runHook({ agent_id: "a", agent_type: "general-purpose", cwd: dir, transcript_path: t }, { CLAUDE_PLUGIN_DATA: dir, CLAUDE_PROJECT_DIR: dir }), "");
});

// --- Integration fixture plumbing (reused from session-start.test.js) ---

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

// --- Integration test ---

test(
  "integration: injects a task-relevant memory",
  { skip: !existsSync(LOCAL_SERVER) && "build topodb-mcp first (cargo build -p topodb-mcp)" },
  async () => {
    const dataDir = mkLocalServerDataDir("topodb-subagent-start-");
    const projectDir = mkdtempSync(path.join(tmpdir(), "topodb-subagent-start-proj-"));
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

      // Seed a distinctive memory
      const seeder = await connectForProject({ projectDir, dataDir });
      assert.ok(seeder, `failed to connect to the seeded broker; stderr: ${brokerErr}`);
      await seeder.call("remember", { content: "the widget parser lives in widget.rs", entities: ["Widget"] });
      seeder.close();

      // Run the hook with a transcript that queries for that content
      const { dir: _pd, t: transcript } = tmpTranscript("where does the widget parser live");
      const out = await runHook(
        { agent_id: "a1", agent_type: "general-purpose", cwd: projectDir, transcript_path: transcript },
        { CLAUDE_PLUGIN_DATA: dataDir, CLAUDE_PROJECT_DIR: projectDir },
      );
      assert.ok(out, `hook should produce output; broker stderr: ${brokerErr}`);
      const parsed = JSON.parse(out);
      assert.equal(parsed.hookSpecificOutput.hookEventName, "SubagentStart");
      assert.match(parsed.hookSpecificOutput.additionalContext, /widget parser/);
    } finally {
      if (broker) broker.kill();
      rmSync(dataDir, { recursive: true, force: true });
      rmSync(projectDir, { recursive: true, force: true });
    }
  },
);
