// test/extension.test.ts
import { test } from "node:test";
import assert from "node:assert/strict";
import registerExtension from "../src/extension.ts";

type Captured = { execute: (...args: any[]) => Promise<any> };

function loadTool(): Captured {
  let captured: Captured | undefined;
  const pi: any = {
    registerTool(def: Captured) {
      captured = def;
    },
    on() {},
  };
  registerExtension(pi);
  if (!captured) throw new Error("registerTool was never called");
  return captured;
}

test("rejects when both action:list and tool are provided", async () => {
  const tool = loadTool();
  const res: any = await tool.execute(
    "call-1",
    { action: "list", tool: "search_memories", args: {} },
    undefined,
    undefined,
    {} as any,
  );
  assert.equal(res.details?.error, "bad-params");
  assert.match(res.content[0].text, /exactly one of/i);
});

test("still rejects when neither action:list nor tool is provided", async () => {
  const tool = loadTool();
  const res: any = await tool.execute("call-2", {}, undefined, undefined, {} as any);
  assert.equal(res.details?.error, "bad-params");
});
