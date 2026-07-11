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

export class TopodbServer {
  private client?: McpStdioClient;
  private toolCache?: McpTool[];

  constructor(private readonly env: NodeJS.ProcessEnv = process.env) {}

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
    const c = await this.ensure();
    if (!this.toolCache) this.toolCache = await c.listTools();
    return this.toolCache;
  }

  async call(tool: string, args: Record<string, unknown>): Promise<unknown> {
    const c = await this.ensure();
    return c.callTool(tool, args);
  }

  shutdown(): void {
    this.client?.stop();
    this.client = undefined;
  }
}
