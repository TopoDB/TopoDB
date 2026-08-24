// src/warehouse.ts — context-warehouse spool for the pi extension.
//
// A TypeScript port of plugins/core/warehouse-spool.js (+ encodeUlid from
// plugins/core/scope-id.js) plus the pi `tool_result` → canonical-tool map.
// Pure helpers, no pi imports; the only I/O is appendSpool. The event format
// is pinned byte-for-byte to plugins/core by test/warehouse-parity.test.ts —
// change core first, then this file. Redaction/size policy is the Rust
// drain's job. Spec: docs/superpowers/specs/2026-08-24-pi-warehouse-capture-design.md
import { appendFileSync, mkdirSync, readFileSync, statSync } from "node:fs";
import { createHash, randomBytes } from "node:crypto";
import path from "node:path";

export const HARNESS = "pi";
export const SPOOL_HARD_CAP = 4 * 1024 * 1024;
const ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

const off = (v: unknown): boolean => {
  const s = String(v ?? "").trim().toLowerCase();
  return s === "0" || s === "off";
};

/** `TOPODB_RECORD=0` is pi's master recording switch (server-handle.ts);
 * `TOPODB_RECORDING` / `TOPODB_WAREHOUSE` are the switches plugins/core and
 * the Rust drain honour. Any of them turns capture off. */
export function warehouseDisabled(env: NodeJS.ProcessEnv): boolean {
  return env.TOPODB_RECORD === "0" || off(env.TOPODB_RECORDING) || off(env.TOPODB_WAREHOUSE);
}

/** Mirrors `topodb_warehouse::warehouse_dir_for_db`: `TOPODB_WAREHOUSE_DIR`
 * when non-blank, else the db path with its extension replaced by
 * `.warehouse` (`memory.redb` -> `memory.warehouse`, `db` -> `db.warehouse`). */
export function warehouseDirForDb(db: string, env: NodeJS.ProcessEnv): string {
  const o = env.TOPODB_WAREHOUSE_DIR;
  if (o && o.trim()) return o;
  const ext = path.extname(db);
  return (ext ? db.slice(0, -ext.length) : db) + ".warehouse";
}

// ---- .topodb.toml [warehouse] (spec 2026-08-24-pi-warehouse-followups §2) ----

/** Filesystem seam for the toml lookup; tests inject a map. */
export interface WarehouseIo {
  existsFile(p: string): boolean;
  readFile(p: string): string;
}
export const nodeIo: WarehouseIo = {
  existsFile: (p) => {
    try {
      return statSync(p).isFile();
    } catch {
      return false;
    }
  },
  readFile: (p) => readFileSync(p, "utf8"),
};

/** Mirrors topodb-mcp's `nearest_topodb_toml`: from the db's parent directory
 * upward, the first regular file named `.topodb.toml`. Deliberately does not
 * resolve a relative db — Rust walks `Path::parent()` on the literal path, so a
 * relative db checks `<parent>/.topodb.toml` and then `.topodb.toml` (cwd). */
export function nearestTopodbToml(db: string, io: WarehouseIo = nodeIo): string | undefined {
  let dir = path.dirname(db);
  for (;;) {
    const cand = path.join(dir, ".topodb.toml");
    if (io.existsFile(cand)) return cand;
    const parent = path.dirname(dir);
    if (parent === dir) return undefined;
    dir = parent;
  }
}

export interface WarehouseSection {
  enabled: boolean;
  path?: string;
}

/** The two `[warehouse]` keys the extension needs, from a tolerant line-level
 * parse: comments, blank lines, `[table]` / `[[array]]` headers, `key = value`
 * with a bare boolean or a quoted string. Everything else is ignored. (Rust
 * rejects a malformed file entirely and falls back to defaults; this parser
 * may still read a well-formed `[warehouse]` table out of such a file —
 * documented divergence, spec §2.) */
export function parseWarehouseToml(text: string): WarehouseSection {
  const out: WarehouseSection = { enabled: true };
  let section = "";
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const hdr = /^\[\[?\s*([^\]]+?)\s*\]\]?$/.exec(line);
    if (hdr) {
      section = hdr[1];
      continue;
    }
    if (section !== "warehouse") continue;
    const kv = /^([A-Za-z0-9_-]+)\s*=\s*(.+)$/.exec(line);
    if (!kv) continue;
    const key = kv[1];
    const val = kv[2].replace(/\s+#.*$/, "").trim();
    if (key === "enabled") {
      if (val === "true" || val === "false") out.enabled = val === "true";
    } else if (key === "path") {
      const q = /^"(.*)"$|^'(.*)'$/.exec(val);
      const p = q ? (q[1] ?? q[2]) : undefined;
      if (p) out.path = p;
    }
  }
  return out;
}

export interface ResolvedWarehouse {
  enabled: boolean;
  dir: string;
  source: "env" | "toml" | "default";
}

/** Mirrors `topodb_onboarding::resolve_warehouse`: toml `enabled = false`
 * disables regardless of env (Rust returns None before reading env); then
 * `TOPODB_WAREHOUSE=0|off`; dir = non-blank `TOPODB_WAREHOUSE_DIR` → toml
 * `path` (`~/` → HOME / USERPROFILE on win32; relative → the toml's directory)
 * → `<db>.warehouse`. An unreadable toml counts as absent, as in Rust. */
export function resolveWarehouse(db: string, env: NodeJS.ProcessEnv, io: WarehouseIo = nodeIo): ResolvedWarehouse {
  const tomlPath = nearestTopodbToml(db, io);
  let section: WarehouseSection = { enabled: true };
  if (tomlPath) {
    try {
      section = parseWarehouseToml(io.readFile(tomlPath));
    } catch {
      /* unreadable → defaults, as Rust */
    }
  }
  const envDir = env.TOPODB_WAREHOUSE_DIR;
  let dir: string;
  let source: ResolvedWarehouse["source"];
  if (envDir && envDir.trim()) {
    dir = envDir;
    source = "env";
  } else if (tomlPath && section.path) {
    const home = env[process.platform === "win32" ? "USERPROFILE" : "HOME"];
    const p = section.path.startsWith("~/") && home ? path.join(home, section.path.slice(2)) : section.path;
    dir = path.isAbsolute(p) ? p : path.join(path.dirname(tomlPath), p);
    source = "toml";
  } else {
    dir = warehouseDirForDb(db, {});
    source = "default";
  }
  return { enabled: section.enabled && !off(env.TOPODB_WAREHOUSE), dir, source };
}

function encodeUlid(bytes: Uint8Array): string {
  let n = 0n;
  for (const b of bytes) n = (n << 8n) | BigInt(b);
  let out = "";
  for (let i = 25; i >= 0; i--) {
    out = ALPHABET[Number(n & 31n)] + out;
    n >>= 5n;
  }
  return out;
}

export function newUlid(nowMs = Date.now()): string {
  const b = new Uint8Array(16);
  let t = BigInt(Math.max(0, Math.floor(nowMs)));
  for (let i = 5; i >= 0; i--) {
    b[i] = Number(t & 0xffn);
    t >>= 8n;
  }
  b.set(randomBytes(10), 6);
  return encodeUlid(b);
}

export function spoolPath(dir: string, sessionId: string): string {
  const safe = String(sessionId).replace(/[^A-Za-z0-9._-]/g, "_");
  return path.join(dir, "spool", `${safe}-${process.pid}.jsonl`);
}

export function appendSpool(dir: string, sessionId: string, event: object): void {
  const p = spoolPath(dir, sessionId);
  mkdirSync(path.dirname(p), { recursive: true });
  appendFileSync(p, JSON.stringify(event) + "\n");
}

export function simpleDiff(oldStr: unknown, newStr: unknown): string {
  const o = String(oldStr ?? "").split("\n");
  const n = String(newStr ?? "").split("\n");
  if (o.length && o[o.length - 1] === "") o.pop();
  if (n.length && n[n.length - 1] === "") n.pop();
  let head = 0;
  while (head < o.length && head < n.length && o[head] === n[head]) head++;
  let tail = 0;
  while (tail < o.length - head && tail < n.length - head && o[o.length - 1 - tail] === n[n.length - 1 - tail]) tail++;
  const lines = ["--- old", "+++ new"];
  for (const l of o.slice(head, o.length - tail)) lines.push("-" + l);
  for (const l of n.slice(head, n.length - tail)) lines.push("+" + l);
  return lines.join("\n") + "\n";
}

type Rec = Record<string, unknown>;
const isRec = (x: unknown): x is Rec => Boolean(x) && typeof x === "object" && !Array.isArray(x);

/** First `{type:"text"}` block of a content array, or the string itself. */
export function firstText(x: unknown): string | undefined {
  if (typeof x === "string") return x;
  if (Array.isArray(x)) {
    const t = x.find((b) => isRec(b) && b.type === "text" && typeof b.text === "string") as Rec | undefined;
    return t ? (t.text as string) : undefined;
  }
  return undefined;
}

export function responseText(_toolName: string, _toolInput: unknown, resp: unknown): string | undefined {
  if (resp === null || resp === undefined) return undefined;
  if (typeof resp === "string") return resp;
  if (Array.isArray(resp)) return firstText(resp);
  if (typeof resp !== "object") return String(resp);
  const r = resp as Rec;
  if (isRec(r.file) && typeof r.file.content === "string") return r.file.content;
  if (typeof r.stdout === "string") return r.stdout + (r.stderr ? "\n[stderr]\n" + r.stderr : "");
  for (const k of ["content", "result", "output", "text"]) {
    const v = r[k];
    if (typeof v === "string") return v;
    const t = firstText(v);
    if (t !== undefined) return t;
  }
  try {
    return JSON.stringify(resp);
  } catch {
    return undefined;
  }
}

const TYPES: Record<string, string> = {
  Read: "file_read", Bash: "command", Edit: "diff", MultiEdit: "diff", Write: "diff",
  Grep: "tool_output", Glob: "tool_output", WebFetch: "tool_output",
};

function locatorFor(toolName: string, ti: Rec): unknown {
  switch (toolName) {
    case "Read": case "Edit": case "MultiEdit": case "Write": return ti.file_path;
    case "Bash": return ti.command;
    case "Grep": case "Glob": return ti.pattern;
    case "WebFetch": return ti.url;
    default: return undefined;
  }
}

export interface SpoolSource { harness: string; session: string; scope: string; tool: string; cwd?: string; agent?: string }
export interface SpoolArtifact { type: string; locator: string; bytes: number; content?: string; hash?: string }
export interface SpoolMarker { type: string; harness: string; session: string; scope: string; node_ids?: string[] }
export interface SpoolEvent {
  id: string; ts: number; host: string; kind: "artifact" | "marker"; v: 1;
  source?: SpoolSource; artifact?: SpoolArtifact; marker?: SpoolMarker;
}
export interface ArtifactArgs {
  toolName: string; toolInput: unknown; toolResponse: unknown;
  sessionId: string; scope: string; cwd?: string; agent?: string; harness?: string; nowMs?: number;
}
export interface MarkerArgs {
  type: string; sessionId: string; scope: string; nodeIds?: string[]; harness?: string; nowMs?: number;
}

export function artifactEvent(a: ArtifactArgs): (SpoolEvent & { source: SpoolSource; artifact: SpoolArtifact }) | null {
  const { toolName, sessionId, scope, cwd, agent, harness = HARNESS, nowMs = Date.now() } = a;
  const type = TYPES[toolName];
  if (!type) return null;
  const ti: Rec = isRec(a.toolInput) ? a.toolInput : {};
  let text: string | undefined;
  if (toolName === "Edit") text = simpleDiff(ti.old_string, ti.new_string);
  else if (toolName === "MultiEdit") {
    const edits = Array.isArray(ti.edits) ? (ti.edits as unknown[]) : [];
    text = edits.map((e) => simpleDiff(isRec(e) ? e.old_string : undefined, isRec(e) ? e.new_string : undefined)).join("\n");
  } else if (toolName === "Write") text = typeof ti.content === "string" ? ti.content : undefined;
  else text = responseText(toolName, ti, a.toolResponse);
  if (typeof text !== "string") return null;
  const locator = String(locatorFor(toolName, ti) ?? "");
  const artifact: SpoolArtifact = { type, locator, bytes: Buffer.byteLength(text, "utf8") };
  if (artifact.bytes > SPOOL_HARD_CAP) artifact.hash = "sha256:" + createHash("sha256").update(text, "utf8").digest("hex");
  else artifact.content = text;
  const source: SpoolSource = { harness, session: String(sessionId), scope: String(scope), tool: toolName };
  if (cwd) source.cwd = String(cwd);
  if (agent) source.agent = String(agent);
  return { id: newUlid(nowMs), ts: nowMs, host: "", kind: "artifact", v: 1, source, artifact };
}

export function markerEvent(a: MarkerArgs): SpoolEvent & { marker: SpoolMarker } {
  const { type, sessionId, scope, nodeIds = [], harness = HARNESS, nowMs = Date.now() } = a;
  const marker: SpoolMarker = { type, harness, session: String(sessionId), scope: String(scope) };
  if (nodeIds.length) marker.node_ids = nodeIds.map(String);
  return { id: newUlid(nowMs), ts: nowMs, host: "", kind: "marker", v: 1, marker };
}

// ---- pi `tool_result` → canonical vocabulary (spec §5) ----------------------

/** The subset of pi's ToolResultEvent we read. Structural so tests can pass
 * plain objects. */
export interface PiToolResult {
  toolName: string;
  input?: unknown;
  content?: unknown;
  details?: unknown;
  isError?: boolean;
}
export interface Mapped { toolName: string; toolInput: Record<string, unknown>; toolResponse: unknown }

/** Pi builtin tool name → canonical tool name. `ls`, custom tools, MCP tools
 * and this extension's own `topodb` tool are absent on purpose: unknown shapes
 * are dropped, never guessed. */
export const PI_TOOL_NAMES: Record<string, string> = {
  bash: "Bash", read: "Read", edit: "MultiEdit", write: "Write", grep: "Grep", find: "Glob",
};

/** Canonical tools whose artifact text is the tool result itself. */
const TEXT_FROM_RESULT = new Set(["Bash", "Read", "Grep", "Glob"]);

export function fromPiToolResult(ev: PiToolResult): Mapped | null {
  if (ev.isError) return null;
  const toolName = PI_TOOL_NAMES[String(ev.toolName ?? "")];
  if (!toolName) return null;
  const input: Rec = isRec(ev.input) ? ev.input : {};
  const toolResponse = firstText(ev.content);
  // Bash/Read/Grep/Glob artifacts ARE the result text; no text block (an
  // image read) means nothing to land. Write/MultiEdit take their text from
  // the input, so they are kept regardless.
  if (toolResponse === undefined && TEXT_FROM_RESULT.has(toolName)) return null;
  switch (toolName) {
    case "Bash": return { toolName, toolInput: { command: input.command }, toolResponse };
    case "Read": return { toolName, toolInput: { file_path: input.path }, toolResponse };
    case "Write": return { toolName, toolInput: { file_path: input.path, content: input.content }, toolResponse };
    case "Grep": case "Glob": return { toolName, toolInput: { pattern: input.pattern }, toolResponse };
    case "MultiEdit": {
      const raw = Array.isArray(input.edits) ? (input.edits as unknown[]) : [];
      const edits = raw
        .filter((e): e is Rec => isRec(e) && typeof e.oldText === "string" && typeof e.newText === "string")
        .map((e) => ({ old_string: e.oldText, new_string: e.newText }));
      if (!edits.length) return null; // junk empty diff — drop, don't guess
      return { toolName, toolInput: { file_path: input.path, edits }, toolResponse };
    }
    default: return null;
  }
}

// ---- memory_write marker ids (spec §6) --------------------------------------

function normalizeResult(r: unknown): Rec | undefined {
  let v: unknown = r;
  if (Array.isArray(v)) v = firstText(v);
  if (typeof v === "string") {
    try { v = JSON.parse(v); } catch { return undefined; }
  }
  return isRec(v) ? v : undefined;
}

/** Same rule as plugins/core `recordMemoryWrite`: 26-char ids among
 * `memory_id`, `id`, `node.id`, `memory.id`, deduped, order preserved. */
export function memoryWriteIds(result: unknown): string[] {
  const r = normalizeResult(result);
  if (!r) return [];
  const cands = [r.memory_id, r.id, isRec(r.node) ? r.node.id : undefined, isRec(r.memory) ? r.memory.id : undefined];
  return [...new Set(cands.filter((s): s is string => typeof s === "string" && s.length === 26))];
}
