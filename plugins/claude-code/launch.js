#!/usr/bin/env node
// The MCP entry point. `.mcp.json` is static and cannot compute a hash, so this
// shim derives the project scope at spawn time, makes sure the database
// directory and the server exist, then hands stdio to topodb-mcp.
//
// A shim, NOT a proxy: Claude Code speaks MCP natively, so there is no protocol
// code here — unlike the Pi extension, which had to bridge MCP to Pi's tool API.
//
// Nothing is exported: the pure part lives in server-args.js, so this file never
// needs an "am I the entry module?" guard (which misfires on Windows casing and
// would leave the server silently never starting).
import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { serverArgs } from "./server-args.js";

// The npm version of the server this plugin is built against. Pinned, not
// floating: a server whose tool surface moved under us is worse than one that is
// a version behind. Bump deliberately — and see Task 5 Step 4, this WILL rot.
const SERVER_VERSION = "0.0.5";
const SERVER_PKG = "@topodb/topodb-mcp";

/**
 * Resolve the server's launcher, installing it into the plugin's persistent data
 * directory on first run. CLAUDE_PLUGIN_DATA survives plugin updates, which is
 * exactly what a node_modules cache (and the database) needs.
 */
function resolveServer(dataDir) {
  const require = createRequire(import.meta.url);
  const entry = `${SERVER_PKG}/bin/topodb-mcp.js`;
  // Resolve ONLY from the data dir's node_modules. Do NOT add this file's own
  // directory (`import.meta.url`'s dirname) to `paths`: in this repo,
  // plugins/claude-code/node_modules/@topodb/topodb-mcp is a devDependency
  // used by the e2e test, so resolving against `here` would silently succeed
  // even when `dataDir` is completely empty, masking the first-run install
  // path entirely. A distributed plugin ships without a node_modules of its
  // own (gitignored, not published), so `here` was never load-bearing there —
  // it only hid bugs in local dev/test.
  const paths = [path.join(dataDir, "node_modules")];

  const installedVersion = () => {
    try {
      const pkgJson = require.resolve(`${SERVER_PKG}/package.json`, { paths });
      // readFileSync + JSON.parse, NOT require(pkgJson): require() caches by
      // resolved path, so a second call after `install()` rewrites the file
      // would return the pre-install value from the module cache and make a
      // successful reinstall look like it silently failed.
      return JSON.parse(readFileSync(pkgJson, "utf8")).version;
    } catch {
      return null;
    }
  };

  const install = () => {
    // First run (or stale version): fetch the pinned server into the data
    // dir. Fail loudly — silently falling back to some other version on the
    // machine is worse than not starting, because the tool surface would be
    // wrong in ways nobody sees.
    //
    // stdout MUST stay "ignore": this process's own stdout is the MCP
    // JSON-RPC stream Claude Code reads. If a future edit "helpfully"
    // changes this to "inherit" (to show install progress), npm's chatter
    // would be interleaved into that stream and corrupt the protocol on
    // every first run. stderr is safe to inherit for diagnostics.
    const res = spawnSync(
      process.platform === "win32" ? "npm.cmd" : "npm",
      ["install", "--prefix", dataDir, "--no-audit", "--no-fund", `${SERVER_PKG}@${SERVER_VERSION}`],
      { stdio: ["ignore", "ignore", "inherit"] },
    );
    if (res.status !== 0) {
      throw new Error(`failed to install ${SERVER_PKG}@${SERVER_VERSION} into ${dataDir}`);
    }
  };

  let version = installedVersion();
  if (version !== SERVER_VERSION) {
    // Either nothing is resolvable yet (first run) or a stale/foreign
    // version is sitting in dataDir (e.g. after a SERVER_VERSION bump).
    // "resolve succeeded" is not "the pinned version is installed" — verify
    // it, don't assume it.
    install();
    version = installedVersion();
    if (version !== SERVER_VERSION) {
      throw new Error(
        `expected ${SERVER_PKG}@${SERVER_VERSION} in ${dataDir}, found ${version ?? "nothing"} after install`,
      );
    }
  }

  return require.resolve(entry, { paths });
}

// CLAUDE_PROJECT_DIR is the repo root Claude Code is working in.
const projectDir = process.env.CLAUDE_PROJECT_DIR ?? process.cwd();
const dataDir = process.env.CLAUDE_PLUGIN_DATA;
if (!dataDir) {
  throw new Error("CLAUDE_PLUGIN_DATA is not set; refusing to guess where to put the database");
}

// topodb-mcp creates the .redb file but treats a missing parent directory as an
// error. This is the exact bug that shipped in @topodb/pi 0.0.1 and made the db
// fail to come up in a fresh project. Do not remove.
mkdirSync(dataDir, { recursive: true });

const child = spawn(
  process.execPath,
  [resolveServer(dataDir), ...serverArgs({ projectDir, dataDir })],
  { stdio: "inherit" }, // this process is a pipe; the server owns the MCP stream
);
child.on("exit", (code) => process.exit(code ?? 1));
child.on("error", (err) => {
  // Without this, a failed spawn (e.g. a bad resolved path, or node missing
  // from PATH) fires an unhandled "error" event and crashes with an opaque
  // stack trace instead of a message that names what failed.
  console.error(`topodb-mcp failed to start: ${err.message}`);
  process.exit(1);
});
