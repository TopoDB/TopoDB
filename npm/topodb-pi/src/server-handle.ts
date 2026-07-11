// src/server-handle.ts
import { createRequire } from "node:module";
import { McpStdioClient, type McpTool } from "./mcp-client.ts";

const require = createRequire(import.meta.url);

export class TopodbServer {
  private client?: McpStdioClient;
  private toolCache?: McpTool[];

  constructor(private readonly env: NodeJS.ProcessEnv = process.env) {}

  static resolveLauncher(): string {
    return require.resolve("@topodb/topodb-mcp/bin/topodb-mcp.js");
  }

  private args(): string[] {
    const db = this.env.TOPODB_DB || ".topodb/memory.redb";
    const scope = this.env.TOPODB_SCOPE || "shared";
    return [TopodbServer.resolveLauncher(), "--db", db, "--scope", scope];
  }

  private async ensure(): Promise<McpStdioClient> {
    if (this.client?.running) return this.client;
    this.client = new McpStdioClient(this.args());
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
