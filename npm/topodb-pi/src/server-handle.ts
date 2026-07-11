// src/server-handle.ts
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { McpStdioClient, type McpTool } from "./mcp-client.ts";

const require = createRequire(import.meta.url);

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
    const args = [TopodbServer.resolveLauncher(), "--db", db, "--scope", scope];
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
