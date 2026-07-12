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
import { mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
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
  // fileURLToPath, NOT `new URL(...).pathname` — on Windows the latter yields
  // "/C:/Users/..." (leading slash, percent-encoded spaces), which no fs or
  // resolver call accepts.
  const here = path.dirname(fileURLToPath(import.meta.url));
  const paths = [path.join(dataDir, "node_modules"), here];
  try {
    return require.resolve(entry, { paths });
  } catch {
    // First run: fetch the server into the data dir. Fail loudly — silently
    // falling back to some other version on the machine is worse than not
    // starting, because the tool surface would be wrong in ways nobody sees.
    const res = spawnSync(
      process.platform === "win32" ? "npm.cmd" : "npm",
      ["install", "--prefix", dataDir, "--no-audit", "--no-fund", `${SERVER_PKG}@${SERVER_VERSION}`],
      { stdio: ["ignore", "ignore", "inherit"] },
    );
    if (res.status !== 0) {
      throw new Error(`failed to install ${SERVER_PKG}@${SERVER_VERSION} into ${dataDir}`);
    }
    return require.resolve(entry, { paths });
  }
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
