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
import { existsSync, mkdirSync, readFileSync, rmdirSync, statSync } from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { serverArgs, SERVER_VERSION } from "./server-args.js";
import { socketPathFor } from "./ipc.js";
import { serveDegraded } from "./degraded.js";

const SERVER_PKG = "@topodb/topodb-mcp";

const LOCK_STALE_MS = 5 * 60 * 1000; // a lock this old outlived any real npm install; treat it as abandoned by a killed process
// 30s, not "a few": this install pulls a platform-specific native binary via
// optionalDependencies, which can take longer than a couple seconds on a cold
// npm cache or a slow link. Long enough to not false-positive a healthy
// install, short enough that a genuinely wedged peer still fails fast-ish.
const LOCK_WAIT_TIMEOUT_MS = 30_000;
const LOCK_POLL_INTERVAL_MS = 200;

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

  // npm is not safe against two processes installing into the same --prefix
  // at once. Two Claude Code sessions launched at the same moment both hit
  // "nothing installed yet" and would otherwise both run `npm install
  // --prefix dataDir` concurrently, which can leave a partial/corrupt
  // node_modules for whichever one loses the race. mkdirSync is atomic
  // (throws EEXIST if the directory already exists), which is enough to use
  // as a cross-process lock without a new dependency.
  const lockDir = path.join(dataDir, ".install.lock");

  // Returns true if we now own the lock, false if someone else legitimately
  // holds it (caller should wait, not install).
  const acquireLock = () => {
    for (;;) {
      try {
        mkdirSync(lockDir);
        return true;
      } catch (err) {
        if (err.code !== "EEXIST") throw err;
        let ageMs;
        try {
          ageMs = Date.now() - statSync(lockDir).mtimeMs;
        } catch {
          // The lock vanished between our mkdirSync and this stat (the
          // holder just finished and cleaned up) — retry acquiring it.
          continue;
        }
        if (ageMs > LOCK_STALE_MS) {
          // A lock this old was almost certainly abandoned by a process that
          // got killed mid-install, not one still legitimately installing.
          // Without this, a single killed process would brick first-run for
          // every future session forever.
          try {
            rmdirSync(lockDir);
          } catch {
            // Lost the cleanup race to another process doing the same thing;
            // either way, retry.
          }
          continue;
        }
        return false;
      }
    }
  };

  // Block (this whole script is synchronous) until the lock holder's install
  // finishes, one way or another.
  const waitForInstall = () => {
    const deadline = Date.now() + LOCK_WAIT_TIMEOUT_MS;
    const sleeper = new Int32Array(new SharedArrayBuffer(4));
    for (;;) {
      if (installedVersion() === SERVER_VERSION) return;
      if (!existsSync(lockDir)) {
        // The holder released the lock — its install finished — but the
        // pinned version still isn't resolvable, so it failed. Don't sit out
        // the rest of the timeout waiting on an install that already ended.
        throw new Error(
          `a concurrent install of ${SERVER_PKG}@${SERVER_VERSION} into ${dataDir} finished without producing that version; it likely failed (check that session's stderr)`,
        );
      }
      if (Date.now() >= deadline) {
        throw new Error(
          `timed out after ${LOCK_WAIT_TIMEOUT_MS}ms waiting for a concurrent install of ${SERVER_PKG}@${SERVER_VERSION} into ${dataDir} to finish`,
        );
      }
      // Atomics.wait blocks the calling thread without spinning; Node (unlike
      // browsers) allows this on the main thread, and there is no async
      // runtime here to yield to anyway (spawnSync below blocks it too).
      Atomics.wait(sleeper, 0, 0, LOCK_POLL_INTERVAL_MS);
    }
  };

  const install = () => {
    if (!acquireLock()) {
      waitForInstall();
      return;
    }
    try {
      installLocked();
    } finally {
      // A crash between mkdirSync and here would skip this and wedge the
      // lock — that's what the staleness check in acquireLock() is for.
      rmdirSync(lockDir);
    }
  };

  const installLocked = () => {
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
      // res.error (e.g. ENOENT when npm itself is missing/not on PATH) is the
      // only signal a user gets when first-run install fails offline, behind
      // a proxy, or on a machine without npm — surface it instead of a bare
      // "it failed" with no cause.
      throw new Error(
        `failed to install ${SERVER_PKG}@${SERVER_VERSION} into ${dataDir}: ${res.error?.message ?? `npm exited ${res.status}`}`,
      );
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

// This process is no longer the server: it is a thin client of the broker
// that owns memory.redb. See specs/2026-07-12-plugin-broker-design.md.
const args = serverArgs({ projectDir, dataDir });
const dbPath = args[args.indexOf("--db") + 1];
const sock = socketPathFor(dbPath);

const tryConnect = () =>
  new Promise((res) => {
    const c = net.connect(sock);
    c.on("connect", () => res(c));
    c.on("error", () => res(null));
  });

/** Once connected, this process is a dumb byte pipe: the broker does the id
 *  rewriting, so the client's JSON-RPC passes through untouched. */
function relay(conn) {
  process.stdin.pipe(conn);
  conn.pipe(process.stdout);
  conn.on("close", () => process.exit(0));
}

let conn = await tryConnect();
let degradedReason = null;

if (!conn) {
  // No broker. Become one — or race another shim doing the same. Both spawn;
  // redb's lock decides which survives (see broker.js). Then connect to the
  // winner.
  try {
    resolveServer(dataDir); // ensure the server is installed BEFORE the broker needs it
    const brokerPath = path.join(path.dirname(fileURLToPath(import.meta.url)), "broker.js");
    spawn(process.execPath, [brokerPath, ...args], {
      detached: true,
      stdio: "ignore",
    }).unref();

    for (let i = 0; i < 25 && !conn; i++) {
      await new Promise((r) => setTimeout(r, 200));
      conn = await tryConnect();
    }
    if (!conn) {
      degradedReason = "could not reach or start the topodb broker; see broker.log in the plugin data dir";
    }
  } catch (err) {
    // resolveServer (install) or the spawn itself failed outright. This must
    // NOT crash the process: a failed MCP server is nearly invisible in
    // Claude Code while the skill keeps telling the agent to call memory
    // tools. Explain the failure instead of dying — see design §5.
    degradedReason = err.message;
  }
}

if (conn) relay(conn);
else serveDegraded(degradedReason ?? "could not reach or start the topodb broker; see broker.log in the plugin data dir");
