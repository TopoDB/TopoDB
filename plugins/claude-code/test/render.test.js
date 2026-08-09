import { test } from "node:test";
import assert from "node:assert/strict";
import { renderMemoryLines } from "../hooks/render.js";

test("renderMemoryLines: seeds header, truncates content, formats age/entities", () => {
  const lines = renderMemoryLines(
    [{ content: "a".repeat(200), entities: ["X", "Y"], ageMs: 2 * 86400000 }],
    "## H",
    6000,
  );
  assert.equal(lines[0], "## H");
  assert.match(lines[1], /^- a{139}… \[entities: X, Y\] \(2d ago\)$/);
});

test("renderMemoryLines: today when ageMs < 1 day, no entities clause when empty", () => {
  const lines = renderMemoryLines([{ content: "hi", entities: [], ageMs: 0 }], "## H", 6000);
  assert.equal(lines[1], "- hi (today)");
});

test("renderMemoryLines: stops before exceeding charCap (header counts toward cap)", () => {
  const many = Array.from({ length: 60 }, () => ({ content: "x".repeat(300), entities: [], ageMs: 1000 }));
  const out = renderMemoryLines(many, "## TopoDB memory (this project)", 6000).join("\n");
  assert.ok(out.length <= 6000, `len ${out.length}`);
  assert.ok(out.length > 3000, "should include several lines, not just the header");
});
