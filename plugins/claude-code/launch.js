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
import { existsSync, mkdirSync, readFileSync } from "node:fs";
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
    const npmArgs = ["install", "--prefix", dataDir, "--no-audit", "--no-fund", `${SERVER_PKG}@${SERVER_VERSION}`];
    const stdio = ["ignore", "ignore", "inherit"];
    let res;
    if (process.platform === "win32") {
      // DO NOT spawn "npm.cmd" directly without shell:true. On Node
      // 20.12+/22+/24 (the CVE-2024-27980 hardening) spawning a .cmd/.bat
      // file with an argv array and no shell throws EINVAL — this is exactly
      // the bug this comment exists to prevent from coming back. Every
      // Windows user's first-run install broke on this before the fix below.
      //
      // The naive counter-fix — just add shell:true — trades EINVAL for a
      // WORSE, silent bug: with shell:true, Node only *concatenates* argv
      // into a command string, it does not quote it. CLAUDE_PLUGIN_DATA
      // resolves under the user's home directory, which routinely contains
      // spaces (e.g. "C:\Users\Andrew Smith\..."); unquoted, cmd.exe splits
      // that path into separate words and npm installs into a truncated,
      // WRONG directory with exit code 0 — no error, just data silently
      // landing somewhere else. Verified empirically while fixing this: an
      // unquoted `--prefix "C:\...\space test dir"` landed in
      // "C:\...\space\node_modules" instead.
      //
      // Instead: run node itself against npm's own CLI entry point. This is
      // NOT a hack — it's the same file npm.cmd itself execs (see npm.cmd's
      // own `%~dp0\node_modules\npm\bin\npm-cli.js` resolution, where %~dp0
      // is npm.cmd's own directory, i.e. the same directory as node.exe).
      // Spawning it via [process.execPath, npmCliJs, ...args] needs no shell
      // at all, so Node's normal (correct, no-shell) Windows argv escaping
      // applies and spaced paths just work — verified empirically too.
      //
      // Fall back to shell:true with each arg manually double-quoted only if
      // that bundled npm-cli.js isn't where every standard Windows Node
      // install (official installer, nvm-windows, volta, scoop, ...) puts
      // it — i.e. only for a genuinely unusual npm layout.
      const npmCliJs = path.join(path.dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");
      if (existsSync(npmCliJs)) {
        res = spawnSync(process.execPath, [npmCliJs, ...npmArgs], { stdio });
      } else {
        const quote = (s) => `"${String(s).replace(/"/g, '\\"')}"`;
        res = spawnSync("npm.cmd", npmArgs.map(quote), { shell: true, stdio });
      }
    } else {
      // POSIX: "npm" is a plain shebang script, not a .cmd — no EINVAL, no
      // shell needed, argv spaces are handled correctly by spawnSync as-is.
      res = spawnSync("npm", npmArgs, { stdio });
    }
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
