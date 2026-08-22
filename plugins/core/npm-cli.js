// Resolve how to invoke npm without depending on a login-shell PATH.
//
// Cursor (and some Claude Code MCP spawns) run `node launch.js` with a stripped
// PATH that has `node` (resolved at spawn) but not `npm`. POSIX launch used to
// call `spawnSync("npm", …)` and first-run then ENOENT → serveDegraded with
// zero tools while Settings still showed the server as "connected".
//
// Windows already ran npm-cli.js next to node.exe. POSIX now prefers the npm
// binary sitting next to `process.execPath`, then the unix prefix's npm-cli.js,
// then PATH `npm` as last resort.
import fs from "node:fs";
import path from "node:path";

/**
 * @param {{ execPath: string, platform?: NodeJS.Platform, existsSync?: (p: string) => boolean }} opts
 * @returns {{ command: string, args: string[], shell: boolean, quoteArgs?: boolean }}
 */
export function resolveNpmSpawn({
  execPath,
  platform = process.platform,
  existsSync = (p) => fs.existsSync(p),
}) {
  const p = platform === "win32" ? path.win32 : path.posix;
  const dir = p.dirname(execPath);

  if (platform === "win32") {
    const cli = p.join(dir, "node_modules", "npm", "bin", "npm-cli.js");
    if (existsSync(cli)) return { command: execPath, args: [cli], shell: false };
    return { command: "npm.cmd", args: [], shell: true, quoteArgs: true };
  }

  const sibling = p.join(dir, "npm");
  if (existsSync(sibling)) return { command: sibling, args: [], shell: false };

  const unixCli = p.join(dir, "..", "lib", "node_modules", "npm", "bin", "npm-cli.js");
  if (existsSync(unixCli)) return { command: execPath, args: [unixCli], shell: false };

  const nextToNode = p.join(dir, "node_modules", "npm", "bin", "npm-cli.js");
  if (existsSync(nextToNode)) return { command: execPath, args: [nextToNode], shell: false };

  return { command: "npm", args: [], shell: false };
}
