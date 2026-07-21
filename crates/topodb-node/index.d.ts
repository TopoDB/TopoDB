/** Wire shape from topodb-json `node_to_json`; shared with Python binding and CLI. */
export interface NodeRecord {
  id: string
  scope: string
  label: string
  props: unknown
}

/** Wire shape from topodb-json `edge_to_json`; shared with Python binding and CLI. */
export interface EdgeRecord {
  id: string
  scope: string
  type: string
  from: string
  to: string
  props: unknown
  valid_from: number
  valid_to: number | null
}

export interface Subgraph {
  nodes: NodeRecord[]
  edges: EdgeRecord[]
}

export interface ScoredNode {
  node: NodeRecord
  score: number
}

export interface SuggestedLink {
  node: NodeRecord
  score: number
  commonNeighbors: string[]
  structural: number
  semantic: number
}

export interface ChangeEvent {
  seq: number
  op: unknown
}

export interface AccessStats {
  accessCount: number
  lastAccessedAt: number | null
}

export interface StorageTableReport {
  table: string
  rows: number
  keyBytes: number
  valueBytes: number
}

export type ErrorCode = 'STORAGE' | 'ENCODING' | 'REJECTED' | 'COMPACTED' | 'CLOSED' | 'UNSUPPORTED_FORMAT'

export class Subscription {
  next(timeoutMs?: number): Promise<ChangeEvent | null>
  close(): void
  [Symbol.asyncIterator](): AsyncIterator<ChangeEvent>
}

export class TopoDB {
  static open(path: string): Promise<TopoDB>
  static openWith(path: string, indexSpec: unknown): Promise<TopoDB>
  static openStored(path: string): Promise<TopoDB>
  static openWithOptions(path: string, indexSpec: unknown, cacheSizeBytes?: number): Promise<TopoDB>
  formatVersion(): Promise<number>
  submit(commands: unknown, defaultScope?: string | null, nowMs?: number): Promise<{
    firstSeq: number
    lastSeq: number
    ids: (string | null)[]
  }>
  node(scopes: string[], id: string): Promise<NodeRecord | null>
  nodesByLabel(scopes: string[], label: string): Promise<NodeRecord[]>
  nodesByLabelNewest(scopes: string[], label: string, k: number): Promise<NodeRecord[]>
  nodesByProp(scopes: string[], label: string, prop: string, value: unknown): Promise<NodeRecord[]>
  nodesByPropNormalized(scopes: string[], label: string, prop: string, value: unknown): Promise<NodeRecord[]>
  nodesByFloatRange(scopes: string[], prop: string, min: number, max: number): Promise<NodeRecord[]>
  edgesFrom(scopes: string[], from: string, opts?: { to?: string; type?: string; openOnly?: boolean }): Promise<EdgeRecord[]>
  allEdgesBetween(from: string, to: string): Promise<EdgeRecord[]>
  openEdgesBetween(from: string, to: string): Promise<string[]>
  traverse(scopes: string[], seeds: string[], maxHops: number, opts?: { edgeTypes?: string[]; direction?: 'out' | 'in' | 'both'; asOf?: number }): Promise<Subgraph>
  searchText(scopes: string[], query: string, k: number, opts?: { recencyWeight?: number; recencyHalfLifeMs?: number; nowMs?: number }): Promise<ScoredNode[]>
  searchVector(scopes: string[], model: string, vector: number[], k: number, candidates?: string[]): Promise<ScoredNode[]>
  recall(scopes: string[], query: string, k: number, opts?: { vector?: { model: string; vector: number[] }; expansions?: Array<[string, string[]]>; graphBoost?: boolean; labels?: string[]; nowMs?: number }): Promise<ScoredNode[]>
  suggestLinks(scopes: string[], node: string, k: number, opts?: { model?: string; asOf?: number; minSemanticSimilarity?: number }): Promise<SuggestedLink[]>
  subscribe(capacity: number): Subscription
  opsSince(seq: number): Promise<ChangeEvent[]>
  currentSeq(): Promise<number>
  compactOps(keepFrom: number): Promise<void>
  indexSpec(): Promise<unknown>
  storageReport(): Promise<StorageTableReport[]>
  accessStats(scopes: string[], id: string): Promise<AccessStats | null>
  rebuildStateFromOps(): Promise<void>
  debugDumpNodes(): Promise<unknown[]>
  debugDumpEdges(): Promise<unknown[]>
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
