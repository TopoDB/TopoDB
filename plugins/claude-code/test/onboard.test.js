// Unit tests for onboard.js: upsertFence (JS mirror of Rust fence.rs) and
// injectPointer (best-effort CLAUDE.md writer).
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { upsertFence, injectPointer } from "../hooks/onboard.js";

const START = "<!-- topodb:pointer:start version=";
const END = "<!-- topodb:pointer:end -->";
const block = (v) => `${START}${v} -->\nBODY v${v}\n${END}\n`;

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

test("upsertFence: reversed markers order is skipped", () => {
  const existing = `${END}\n${START}1 -->\nBODY v1\n${END}\n`;
  const { text, outcome } = upsertFence(existing, block(2), 2);
  assert.equal(outcome, "skipped");
  assert.equal(text, existing);
});

test("injectPointer: writes CLAUDE.md and is idempotent; missing tool doesn't throw", async () => {
  const proj = mkdtempSync(path.join(tmpdir(), "cc-onb-"));
  try {
    const goodClient = { call: async () => ({ pointer: block(1), version: 1 }) };
    await injectPointer({ projectDir: proj, client: goodClient });
    const a = readFileSync(path.join(proj, "CLAUDE.md"), "utf8");
    assert.ok(a.includes("topodb:pointer:start"));

    await injectPointer({ projectDir: proj, client: goodClient }); // idempotent
    assert.equal(readFileSync(path.join(proj, "CLAUDE.md"), "utf8"), a);

    const badClient = { call: async () => { throw new Error("no tool"); } };
    await injectPointer({ projectDir: proj, client: badClient }); // must not throw
    assert.equal(readFileSync(path.join(proj, "CLAUDE.md"), "utf8"), a); // unchanged
  } finally {
    rmSync(proj, { recursive: true, force: true });
  }
});

test("injectPointer: creates CLAUDE.md when absent", async () => {
  const proj = mkdtempSync(path.join(tmpdir(), "cc-onb2-"));
  try {
    assert.ok(!existsSync(path.join(proj, "CLAUDE.md")));
    const client = { call: async () => ({ pointer: block(1), version: 1 }) };
    await injectPointer({ projectDir: proj, client });
    assert.ok(existsSync(path.join(proj, "CLAUDE.md")));
  } finally {
    rmSync(proj, { recursive: true, force: true });
  }
});

test("injectPointer: no client is a no-op, never throws", async () => {
  const proj = mkdtempSync(path.join(tmpdir(), "cc-onb3-"));
  try {
    await injectPointer({ projectDir: proj, client: null });
    assert.ok(!existsSync(path.join(proj, "CLAUDE.md")));
  } finally {
    rmSync(proj, { recursive: true, force: true });
  }
});
