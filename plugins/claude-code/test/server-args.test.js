import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { readFileSync } from "node:fs";
import { serverArgs, SERVER_VERSION } from "../server-args.js";
import { projectScopeId } from "../scope-id.js";

test("reads span the project scope AND shared; writes default to the project", () => {
  const args = serverArgs({ projectDir: "/tmp/proj", dataDir: "/data" });
  const scope = projectScopeId("/tmp/proj");

  assert.deepEqual(args, [
    "--db",
    path.join("/data", "memory.redb"),
    "--scope",
    scope,
    "--read-scopes",
    `${scope},shared`,
  ]);
});

test("get_changes is NEVER enabled", () => {
  // The one unscoped read. In a db shared across every project, enabling it
  // would let a session replay every OTHER project's writes into its context.
  // This is the whole reason a global database is safe; guard it explicitly.
  const args = serverArgs({ projectDir: "/tmp/proj", dataDir: "/data" });
  assert.ok(!args.includes("--allow-unscoped-changes"));
});

test("the db lives in the plugin DATA dir, not the plugin ROOT", () => {
  // CLAUDE_PLUGIN_ROOT is replaced on every plugin update. A db written there
  // would be silently discarded on upgrade.
  const args = serverArgs({ projectDir: "/tmp/proj", dataDir: "/data" });
  assert.equal(args[1], path.join("/data", "memory.redb"));
});

const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
test("the server the launcher installs is the server the e2e test validates", () => {
  // SERVER_VERSION (launch.js's pin, via server-args.js) and
  // devDependencies["@topodb/topodb-mcp"] (what the e2e test actually
  // exercises) are two hand-synced copies of the same fact. If they drift,
  // the e2e suite stays green while validating a server version no user ever
  // launches — exactly the drift that shipped @topodb/pi with a stale 0.0.3
  // for a week, unnoticed.
  assert.equal(SERVER_VERSION, pkg.devDependencies["@topodb/topodb-mcp"]);
});
