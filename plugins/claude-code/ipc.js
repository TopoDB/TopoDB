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
 * The first line a shim sends after connecting: which scopes THIS session reads
 * and writes. Everything after it on the connection is ordinary MCP.
 *
 * It has to be sent, not inferred, because the broker cannot know it: the broker
 * owns the database (one per redb lock), but the scope belongs to the *project*,
 * and one database serves every project. A dedicated key — rather than smuggling
 * it through `initialize` — keeps it impossible to confuse with a JSON-RPC
 * message: `hello.jsonrpc` is undefined, so a broker that somehow forwarded one
 * upstream would be rejected rather than silently misread.
 */
export const HELLO_KEY = "topodb/hello";

/** Builds the hello frame. `scope` is the write scope; `readScopes` the read set. */
export function helloFrame({ scope, readScopes }) {
  return JSON.stringify({ [HELLO_KEY]: { scope, read_scopes: readScopes } }) + "\n";
}

/** The `_meta` keys `topodb-mcp` reads a per-request scope override from. These
 * MUST match `META_SCOPE`/`META_READ_SCOPES` in `crates/topodb-mcp/src/server.rs`. */
export const META_SCOPE = "topodb/scope";
export const META_READ_SCOPES = "topodb/read_scopes";

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
