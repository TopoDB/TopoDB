// src/server-handle.ts
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { McpStdioClient, type McpTool } from "./mcp-client.ts";

const require = createRequire(import.meta.url);

/** Recording defaults ON; `TOPODB_RECORD=0` disables it. */
export function recordingEnabled(env: NodeJS.ProcessEnv): boolean {
  return env.TOPODB_RECORD !== "0";
}

/** `--spec` launch args pointing at the bundled episode IndexSpec, or `[]`
 * when recording is disabled. `../spec/` is relative to the COMPILED
 * `dist/server-handle.js` — `spec/` sits at the package root alongside
 * `dist/`, so from `dist/` this resolves correctly both in-repo and once
 * published (see package.json `files`). */
export function episodeSpecArgs(env: NodeJS.ProcessEnv): string[] {
  if (!recordingEnabled(env)) return [];
  const specPath = new URL("../spec/episode-index-spec.json", import.meta.url);
  return ["--spec", fileURLToPath(specPath)];
}

/** Idle milliseconds before the resident child is reaped so redb's exclusive
 * lock frees for other processes. `TOPODB_IDLE_MS=0` disables idle release
 * (always resident); unset/invalid falls back to 30s. */
export function idleMs(env: NodeJS.ProcessEnv): number {
  const raw = env.TOPODB_IDLE_MS;
  if (raw === undefined || raw === "") return 30_000;
  const n = Number(raw);
  return Number.isInteger(n) && n >= 0 ? n : 30_000;
}

export class TopodbServer {
  private client?: McpStdioClient;
  private toolCache?: McpTool[];
  private inflight = 0;
  private idleTimer?: NodeJS.Timeout;

  constructor(private readonly env: NodeJS.ProcessEnv = process.env) {}

  /** Whether a topodb-mcp child is currently resident (holding the db lock). */
  get running(): boolean {
    return this.client?.running ?? false;
  }

  /** The timer arms only when the LAST in-flight op completes, never at op
   * start — a cold spawn can outlast a short idle window and must not be
   * reaped under the call that is paying for it. unref: an armed timer must
   * not keep the host process alive. */
  private beginOp(): void {
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = undefined;
    }
    this.inflight++;
  }

  private endOp(): void {
    this.inflight--;
    const ms = idleMs(this.env);
    if (ms > 0 && this.inflight === 0 && this.running) {
      this.idleTimer = setTimeout(() => {
        this.idleTimer = undefined;
        if (this.inflight === 0) this.shutdown();
      }, ms);
      this.idleTimer.unref?.();
    }
  }

  static resolveLauncher(): string {
    return require.resolve("@topodb/topodb-mcp/bin/topodb-mcp.js");
  }

  private async ensure(): Promise<McpStdioClient> {
    if (this.client?.running) return this.client;
    const db = this.env.TOPODB_DB || ".topodb/memory.redb";
    const scope = this.env.TOPODB_SCOPE || "shared";
    // topodb-mcp creates the db file on open but treats a missing parent
    // directory as a startup error — and the default `.topodb/` won't exist in
    // a fresh project. Create it so the server comes up on first use.
    mkdirSync(dirname(db), { recursive: true });
    const args = [
      TopodbServer.resolveLauncher(),
      "--db",
      db,
      "--scope",
      scope,
      ...episodeSpecArgs(this.env),
    ];
    this.client = new McpStdioClient(args);
    await this.client.start();
    return this.client;
  }

  async list(): Promise<McpTool[]> {
    // The tool list is static per server version — serving the cache without
    // ensure() avoids respawning an idled child just to re-answer discovery.
    if (this.toolCache) return this.toolCache;
    this.beginOp();
    try {
      const c = await this.ensure();
      this.toolCache = await c.listTools();
      return this.toolCache;
    } finally {
      this.endOp();
    }
  }

  async call(tool: string, args: Record<string, unknown>): Promise<unknown> {
    this.beginOp();
    try {
      const c = await this.ensure();
      return await c.callTool(tool, args);
    } finally {
      this.endOp();
    }
  }

  shutdown(): void {
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = undefined;
    }
    this.client?.stop();
    this.client = undefined;
  }
}
