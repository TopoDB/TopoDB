// test/onboard.test.ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { injectPointer, upsertFence } from "../src/onboard.ts";
import { TopodbServer } from "../src/server-handle.ts";
import registerExtension from "../src/extension.ts";

const START = "<!-- topodb:pointer:start version=";
const END = "<!-- topodb:pointer:end -->";
const block = (v: number) => `${START}${v} -->\nBODY v${v}\n${END}\n`;

test("upsertFence: appends when absent", () => {
  const { text, outcome } = upsertFence("# My rules\n", block(1), 1);
  assert.equal(outcome, "injected");
  assert.ok(text.startsWith("# My rules\n"));
  assert.ok(text.includes(block(1)));
});

test("upsertFence: replaces in place on newer version", () => {
  const existing = `top\n${block(1).trimEnd()}\nbottom\n`;
  const { text, outcome } = upsertFence(existing, block(2), 2);
  assert.equal(outcome, "replaced");
  assert.ok(text.includes("BODY v2"));
  assert.ok(!text.includes("BODY v1"));
  assert.ok(text.includes("top\n") && text.includes("bottom\n"));
});

test("upsertFence: unchanged when same or newer version present", () => {
  const existing = `x\n${block(2).trimEnd()}\n`;
  const { text, outcome } = upsertFence(existing, block(2), 2);
  assert.equal(outcome, "unchanged");
  assert.equal(text, existing);
});

test("upsertFence: skips on corrupted single marker", () => {
  const existing = `x\n${START}1 -->\nno end marker here\n`;
  const { text, outcome } = upsertFence(existing, block(1), 1);
  assert.equal(outcome, "skipped");
  assert.equal(text, existing);
});

test("upsertFence: malformed version parses as zero and gets replaced", () => {
  const existing = `top\n${START}abc -->\nGARBLED\n${END}\nbottom\n`;
  const { text, outcome } = upsertFence(existing, block(2), 2);
  assert.equal(outcome, "replaced");
  assert.ok(text.includes("BODY v2"));
  assert.ok(!text.includes("GARBLED"));
  assert.ok(text.includes("top\n") && text.includes("bottom\n"));
});

test("upsertFence: oversized (> u32::MAX) version parses as zero and gets replaced (Rust parity)", () => {
  const existing = `top\n${START}99999999999 -->\nGARBLED\n${END}\nbottom\n`;
  const { text, outcome } = upsertFence(existing, block(5), 5);
  assert.equal(outcome, "replaced");
  assert.ok(text.includes("BODY v5"));
  assert.ok(!text.includes("GARBLED"));
  assert.ok(text.includes("top\n") && text.includes("bottom\n"));
});

test("upsertFence: reversed marker order is skipped", () => {
  const existing = `${END}\n${START}1 -->\nBODY v1\n${END}\n`;
  const { text, outcome } = upsertFence(existing, block(2), 2);
  assert.equal(outcome, "skipped");
  assert.equal(text, existing);
});

test("injectPointer: creates AGENTS.md, idempotent, swallows a throwing call", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "pi-onb-"));
  try {
    const good = {
      call: async () => ({ pointer: block(1), version: 1 }),
    } as unknown as TopodbServer;
    await injectPointer(cwd, good);
    const file = join(cwd, "AGENTS.md");
    assert.ok(existsSync(file));
    const first = readFileSync(file, "utf8");
    assert.ok(first.includes("topodb:pointer:start"));
    assert.ok(first.includes("BODY v1"));

    // Second call with the same version: unchanged, file untouched.
    await injectPointer(cwd, good);
    assert.equal(readFileSync(file, "utf8"), first);

    const bad = {
      call: async () => {
        throw new Error("boom");
      },
    } as unknown as TopodbServer;
    await assert.doesNotReject(injectPointer(cwd, bad));
  } finally {
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("injectPointer: swallows a malformed result shape (no pointer/version)", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "pi-onb-bad-"));
  try {
    const weird = { call: async () => ({ nope: true }) } as unknown as TopodbServer;
    await assert.doesNotReject(injectPointer(cwd, weird));
    assert.equal(existsSync(join(cwd, "AGENTS.md")), false);
  } finally {
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("session_start handler: writes AGENTS.md once, latch prevents a second effect", async () => {
  const handlers = new Map<string, (ev: unknown, ctx: unknown) => Promise<unknown> | unknown>();
  const pi = {
    on(event: string, handler: (ev: unknown, ctx: unknown) => Promise<unknown> | unknown) {
      handlers.set(event, handler);
    },
    registerTool() {},
  } as unknown as Parameters<typeof registerExtension>[0];

  let calls = 0;
  const original = TopodbServer.prototype.call;
  TopodbServer.prototype.call = async function () {
    calls++;
    return { pointer: block(1), version: 1 };
  };

  const cwd = mkdtempSync(join(tmpdir(), "pi-onb-ext-"));
  try {
    registerExtension(pi);
    const handler = handlers.get("session_start");
    assert.ok(handler, "session_start handler was registered");

    await handler!({ type: "session_start" }, { cwd });
    const file = join(cwd, "AGENTS.md");
    assert.ok(existsSync(file));
    const first = readFileSync(file, "utf8");
    assert.equal(calls, 1);

    // Second invocation: latch means no second server.call, file unchanged.
    await handler!({ type: "session_start" }, { cwd });
    assert.equal(calls, 1, "latch prevented a second onboarding_pointer call");
    assert.equal(readFileSync(file, "utf8"), first);
  } finally {
    TopodbServer.prototype.call = original;
    rmSync(cwd, { recursive: true, force: true });
  }
});
