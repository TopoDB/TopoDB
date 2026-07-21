export interface NodeRecord {
  id: string
  label: string
  props: unknown
  createdAt: number
  validFrom: number
  validTo: number | null
}

export interface EdgeRecord {
  id: string
  from: string
  to: string
  type: string
  props: unknown
  createdAt: number
  validFrom: number
  validTo: number | null
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

export class TopoDB {
  static open(path: string): Promise<TopoDB>
  static openWith(path: string, indexSpec: unknown): Promise<TopoDB>
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
