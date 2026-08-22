import { test } from "node:test";
import assert from "node:assert/strict";
import { serverInvocation } from "../launch.js";

// TOPODB_MCP_SERVER_BIN normally names a NATIVE topodb-mcp binary, but the
// launch seam tests point it at a .mjs fake. POSIX execs the shebang; Windows
// cannot spawn a non-PE file (spawn EFTYPE), so script overrides must route
// through the current node — same shape the resolveServer path already uses.
test("serverInvocation: native binary override spawns directly", () => {
  const bin = process.platform === "win32" ? "C:\\t\\topodb-mcp.exe" : "/t/topodb-mcp";
  assert.deepEqual(serverInvocation(bin), { command: bin, preArgs: [] });
});

test("serverInvocation: .mjs/.js/.cjs overrides run via the current node (Windows cannot exec a script)", () => {
  for (const ext of ["mjs", "js", "cjs", "MJS"]) {
    const fake = `/tmp/fake-mcp.${ext}`;
    assert.deepEqual(serverInvocation(fake), { command: process.execPath, preArgs: [fake] });
  }
});
