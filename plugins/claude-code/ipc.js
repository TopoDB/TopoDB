// Transport plumbing shared by the shim and the broker.
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import path from "node:path";

/**
 * The IPC endpoint for a given database. Derived from the db path, so two
 * different databases never share a broker.
 *
 * The socket lives in the OS temp dir, NOT beside the database: a unix socket
 * path is limited to ~104 bytes (`sun_path`), and CLAUDE_PLUGIN_DATA can be
 * arbitrarily deep. Binding would fail with ENAMETOOLONG for reasons no user
 * could diagnose.
 */
export function socketPathFor(dbPath) {
  const h = createHash("sha256").update(path.resolve(dbPath), "utf8").digest("hex").slice(0, 12);
  return process.platform === "win32"
    ? `\\\\.\\pipe\\topodb-${h}`
    : path.join(tmpdir(), `topodb-${h}.sock`);
}

/**
 * Newline-delimited JSON framing — the same framing MCP already uses over
 * stdio, so the broker relays payloads without re-encoding them. Returns a
 * function to feed chunks into; complete lines go to `onLine`, partials are
 * held until the rest arrives.
 */
export function lineReader(onLine) {
  let buf = "";
  return (chunk) => {
    buf += chunk;
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (line) onLine(line);
    }
  };
}
