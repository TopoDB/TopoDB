import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { syncCore } from "../../../scripts/sync-plugin-core.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
test("core/ is byte-identical to plugins/core (run `npm run sync` after editing plugins/core)", () => {
  const { drift } = syncCore({ source: path.join(HERE, "..", "..", "core"), targets: [path.join(HERE, "..", "core")], check: true });
  assert.deepEqual(drift, []);
});
