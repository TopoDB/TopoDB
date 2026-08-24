// src/server-handle.ts
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { McpStdioClient, type McpClientOptions, type McpTool } from "./mcp-client.ts";

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
 * (always resident); unset/invalid falls back to 30s. Values beyond Node's
 * setTimeout range (2^31-1) are clamped — Node would otherwise clamp the
 * delay to 1ms, turning "practically never" into "after every call". */
const MAX_TIMEOUT_MS = 2 ** 31 - 1;
export function idleMs(env: NodeJS.ProcessEnv): number {
  const raw = env.TOPODB_IDLE_MS;
  if (raw === undefined || raw === "") return 30_000;
  const n = Number(raw);
  return Number.isInteger(n) && n >= 0 ? Math.min(n, MAX_TIMEOUT_MS) : 30_000;
}

/** The db the server is launched against — also what the warehouse dir is
 * derived from (warehouse.ts), so the two can never disagree. */
export function dbPath(env: NodeJS.ProcessEnv): string {
  return env.TOPODB_DB || ".topodb/memory.redb";
}

/** The scope label write tools stamp when they omit `scope` — and therefore
 * the label warehouse events carry (spec §7). */
export function scopeLabel(env: NodeJS.ProcessEnv): string {
  return env.TOPODB_SCOPE || "shared";
}

export type TopodbServerOptions = {
  /** Full node-args override for the spawned child. Tests inject stub servers
   * here; production always uses the bundled launcher. */
  launcherArgs?: string[];
  /** Options threaded to the underlying McpStdioClient (timeouts, grace). */
  clientOpts?: McpClientOptions;
};

export class TopodbServer {
  private client?: McpStdioClient;
  private toolCache?: McpTool[];
  private inflight = 0;
  private idleTimer?: NodeJS.Timeout;
  /** Single-flight guard: concurrent cold callers share one spawn+handshake
   * instead of the second one racing a half-initialized client. */
  private starting?: Promise<McpStdioClient>;
  /** Exit promise of the most recently reaped child. A respawn awaits it so
   * the new child never races the dying one for the redb lock. */
  private reaping?: Promise<void>;

  constructor(
    private readonly env: NodeJS.ProcessEnv = process.env,
    private readonly opts: TopodbServerOptions = {},
  ) {}

  /** Resolves when the last reaped child has fully exited (lock released). */
  get whenReaped(): Promise<void> | undefined {
    return this.reaping;
  }

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
    if (!this.starting) {
      this.starting = this.spawnClient().finally(() => {
        this.starting = undefined;
      });
    }
    return this.starting;
  }

  private async spawnClient(): Promise<McpStdioClient> {
    await this.reaping; // let the previous child release the lock first
    const db = dbPath(this.env);
    const scope = scopeLabel(this.env);
    // topodb-mcp creates the db file on open but treats a missing parent
    // directory as a startup error — and the default `.topodb/` won't exist in
    // a fresh project. Create it so the server comes up on first use.
    mkdirSync(dirname(db), { recursive: true });
    const args = this.opts.launcherArgs ?? [
      TopodbServer.resolveLauncher(),
      "--db",
      db,
      "--scope",
      scope,
      ...episodeSpecArgs(this.env),
    ];
    // env last: the server's env-only settings (TOPODB_LOCK_WAIT_MS, ...) must
    // reach the child, which spawn() does not do implicitly.
    const client = new McpStdioClient(args, { ...this.opts.clientOpts, env: this.env });
    try {
      await client.start();
    } catch (e) {
      // A rejected handshake must not leave an alive-but-orphaned child
      // holding the db lock (e.g. initialize timing out during a long
      // first-open migration).
      client.stop();
      throw e;
    }
    this.client = client;
    // A new child may be new code — the cached tool list belongs to the old one.
    this.toolCache = undefined;
    return client;
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
    } catch (e) {
      // Transport-level failure (timeout, child death): the child is wedged
      // or gone — reap it so the lock frees and the next call gets a fresh
      // spawn. App-level errors from a healthy child keep it resident,
      // matching sgh's OnDemandBridge (Tool errors never respawn).
      if ((e as { transport?: boolean })?.transport === true) this.shutdown();
      throw e;
    } finally {
      this.endOp();
    }
  }

  shutdown(): void {
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = undefined;
    }
    if (this.client) {
      this.client.stop();
      this.reaping = this.client.whenExited;
      this.client = undefined;
    }
  }
}
