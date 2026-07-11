// src/mcp-client.ts
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface, type Interface } from "node:readline";

export type McpTool = { name: string; description?: string; inputSchema?: unknown };

type Pending = { resolve: (v: unknown) => void; reject: (e: Error) => void };

export class McpStdioClient {
  private child?: ChildProcessWithoutNullStreams;
  private rl?: Interface;
  private nextId = 1;
  private pending = new Map<number, Pending>();

  constructor(private readonly nodeArgs: string[]) {}

  get running(): boolean {
    return !!this.child && this.child.exitCode === null && !this.child.killed;
  }

  async start(): Promise<void> {
    const child = spawn(process.execPath, this.nodeArgs, {
      stdio: ["pipe", "pipe", "inherit"],
    });
    this.child = child;
    this.rl = createInterface({ input: child.stdout });
    this.rl.on("line", (line) => this.onLine(line));
    child.on("exit", () => this.failAll(new Error("topodb-mcp exited")));

    await this.request("initialize", {
      protocolVersion: "2025-11-25",
      capabilities: {},
      clientInfo: { name: "topodb-pi", version: "0.0.1" },
    });
    this.notify("notifications/initialized");
  }

  async listTools(): Promise<McpTool[]> {
    const r = (await this.request("tools/list")) as { tools?: McpTool[] };
    return r.tools ?? [];
  }

  async callTool(name: string, args: Record<string, unknown>): Promise<unknown> {
    const r = (await this.request("tools/call", { name, arguments: args })) as {
      structuredContent?: unknown;
      content?: unknown;
    };
    return r.structuredContent ?? r.content ?? r;
  }

  stop(): void {
    this.rl?.close();
    this.child?.kill();
    this.failAll(new Error("client stopped"));
  }

  private onLine(line: string): void {
    let msg: any;
    try { msg = JSON.parse(line); } catch { return; }
    if (typeof msg?.id !== "number") return; // notifications/logs
    const p = this.pending.get(msg.id);
    if (!p) return;
    this.pending.delete(msg.id);
    if (msg.error) p.reject(new Error(msg.error.message ?? "MCP error"));
    else p.resolve(msg.result);
  }

  private request(method: string, params?: unknown): Promise<unknown> {
    const id = this.nextId++;
    const line = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child!.stdin.write(line);
    });
  }

  private notify(method: string, params?: unknown): void {
    this.child!.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
  }

  private failAll(err: Error): void {
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();
  }
}
