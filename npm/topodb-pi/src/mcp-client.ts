// src/mcp-client.ts
import { spawn, type ChildProcessByStdio } from "node:child_process";
import { createInterface, type Interface } from "node:readline";
import type { Writable, Readable } from "node:stream";

export type McpTool = { name: string; description?: string; inputSchema?: unknown };

export type McpClientOptions = {
  /** Per-request timeout, in ms. Applies to every request(), including the initial handshake. */
  requestTimeoutMs?: number;
  /** Executable to spawn instead of the current node binary. Mainly for tests. */
  command?: string;
};

const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;

type Pending = { resolve: (v: unknown) => void; reject: (e: Error) => void };

export class McpStdioClient {
  private child?: ChildProcessByStdio<Writable, Readable, null>;
  private rl?: Interface;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private readonly requestTimeoutMs: number;
  private readonly command: string;

  constructor(
    private readonly nodeArgs: string[],
    opts: McpClientOptions = {},
  ) {
    this.requestTimeoutMs = opts.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
    this.command = opts.command ?? process.execPath;
  }

  get running(): boolean {
    return !!this.child && this.child.exitCode === null && !this.child.killed;
  }

  async start(): Promise<void> {
    const child = spawn(this.command, this.nodeArgs, {
      stdio: ["pipe", "pipe", "ignore"],
    });
    this.child = child;
    // A spawn failure (bad executable, EMFILE/EAGAIN, ...) emits 'error' instead of
    // (or in addition to) 'exit'. Without a handler this is an uncaught exception
    // that crashes the host process. Route it through the same failAll() path.
    child.on("error", (e) => this.failAll(e));
    // Writing to stdin after the child has died surfaces as EPIPE here. failAll()
    // (via the 'exit'/'error' handlers above) already rejects pending requests, so
    // this handler exists purely to prevent an unhandled 'error' event from
    // throwing and crashing the host process.
    child.stdin.on("error", () => {});
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
      isError?: boolean;
    };
    // NOTE: tool-level MCP `isError:true` results are currently surfaced as a
    // success (the content is returned as-is). Treating that as a rejection is
    // deferred — accepted v0 limitation, tracked as finding 5.
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
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(
          new Error(`topodb-mcp request timed out after ${this.requestTimeoutMs}ms: ${method}`),
        );
      }, this.requestTimeoutMs);
      timer.unref?.();
      this.pending.set(id, {
        resolve: (v) => {
          clearTimeout(timer);
          resolve(v);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
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
