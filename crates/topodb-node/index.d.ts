export class TopoDB {
  static open(path: string): Promise<TopoDB>
  formatVersion(): Promise<number>
  submit(commands: unknown, defaultScope?: string | null, nowMs?: number): Promise<{
    firstSeq: number
    lastSeq: number
    ids: (string | null)[]
  }>
  close(): void
  [Symbol.dispose](): void
}

export const ops: {
  createEntity(name: string, scope?: string): unknown
  createMemory(content: string, scope?: string): unknown
  createNode(label: string, props?: unknown, scope?: string): unknown
  link(from: string, to: string, type: string, opts?: { props?: unknown; scope?: string; validFrom?: number }): unknown
  setNodeProps(id: string, props: unknown): unknown
  removeNode(id: string): unknown
  closeEdge(id: string, validTo?: number): unknown
  setEmbedding(id: string, model: string, vector: number[]): unknown
}
