const native = require('./native.js')

const CODE_RE = /^\[([A-Z_]+)\] /
function decorate(err) {
  if (err instanceof Error) {
    const m = CODE_RE.exec(err.message)
    if (m) {
      err.code = m[1]
      err.message = err.message.slice(m[0].length)
      if (err.code === 'COMPACTED') {
        const n = /oldest retained seq is (\d+)/.exec(err.message)
        if (n) err.oldest = Number(n[1])
      }
      if (err.code === 'UNSUPPORTED_FORMAT') {
        const n = /version (\d+) .*up to (\d+)/.exec(err.message)
        if (n) { err.found = Number(n[1]); err.supported = Number(n[2]) }
      }
    }
  }
  throw err
}

class TopoDB {
  constructor(inner) {
    this._inner = inner
  }

  static async open(path) {
    try {
      const inner = await native.TopoDB.open(path)
      return new TopoDB(inner)
    } catch (e) {
      decorate(e)
    }
  }

  static async openWith(path, indexSpec) {
    try {
      const inner = await native.TopoDB.openWith(path, indexSpec)
      return new TopoDB(inner)
    } catch (e) {
      decorate(e)
    }
  }

  async formatVersion() {
    try {
      return await this._inner.formatVersion()
    } catch (e) {
      decorate(e)
    }
  }

  async submit(commands, defaultScope, nowMs) {
    try {
      return await this._inner.submit(commands, defaultScope, nowMs)
    } catch (e) {
      decorate(e)
    }
  }

  async node(scopes, id) {
    try {
      return await this._inner.node(scopes, id)
    } catch (e) {
      decorate(e)
    }
  }

  async nodesByLabel(scopes, label) {
    try {
      return await this._inner.nodesByLabel(scopes, label)
    } catch (e) {
      decorate(e)
    }
  }

  async nodesByLabelNewest(scopes, label, k) {
    try {
      return await this._inner.nodesByLabelNewest(scopes, label, k)
    } catch (e) {
      decorate(e)
    }
  }

  async nodesByProp(scopes, label, prop, value) {
    try {
      return await this._inner.nodesByProp(scopes, label, prop, value)
    } catch (e) {
      decorate(e)
    }
  }

  async nodesByPropNormalized(scopes, label, prop, value) {
    try {
      return await this._inner.nodesByPropNormalized(scopes, label, prop, value)
    } catch (e) {
      decorate(e)
    }
  }

  async nodesByFloatRange(scopes, prop, min, max) {
    try {
      return await this._inner.nodesByFloatRange(scopes, prop, min, max)
    } catch (e) {
      decorate(e)
    }
  }

  async edgesFrom(scopes, from, opts) {
    try {
      return await this._inner.edgesFrom(scopes, from, opts)
    } catch (e) {
      decorate(e)
    }
  }

  async allEdgesBetween(from, to) {
    try {
      return await this._inner.allEdgesBetween(from, to)
    } catch (e) {
      decorate(e)
    }
  }

  async openEdgesBetween(from, to) {
    try {
      return await this._inner.openEdgesBetween(from, to)
    } catch (e) {
      decorate(e)
    }
  }

  async traverse(scopes, seeds, maxHops, opts) {
    try {
      return await this._inner.traverse(scopes, seeds, maxHops, opts)
    } catch (e) {
      decorate(e)
    }
  }

  async searchText(scopes, query, k, opts) {
    try {
      return await this._inner.searchText(scopes, query, k, opts)
    } catch (e) {
      decorate(e)
    }
  }

  async searchVector(scopes, model, vector, k, candidates) {
    try {
      return await this._inner.searchVector(scopes, model, vector, k, candidates)
    } catch (e) {
      decorate(e)
    }
  }

  async recall(scopes, query, k, opts) {
    try {
      return await this._inner.recall(scopes, query, k, opts)
    } catch (e) {
      decorate(e)
    }
  }

  async suggestLinks(scopes, node, k, opts) {
    try {
      return await this._inner.suggestLinks(scopes, node, k, opts)
    } catch (e) {
      decorate(e)
    }
  }

  subscribe(capacity) {
    try {
      const sub = this._inner.subscribe(capacity)
      return new Subscription(sub)
    } catch (e) {
      decorate(e)
    }
  }

  async opsSince(seq) {
    try {
      return await this._inner.opsSince(seq)
    } catch (e) {
      decorate(e)
    }
  }

  async currentSeq() {
    try {
      return await this._inner.currentSeq()
    } catch (e) {
      decorate(e)
    }
  }

  async compactOps(keepFrom) {
    try {
      return await this._inner.compactOps(keepFrom)
    } catch (e) {
      decorate(e)
    }
  }

  async indexSpec() {
    try {
      return await this._inner.indexSpec()
    } catch (e) {
      decorate(e)
    }
  }

  async storageReport() {
    try {
      return await this._inner.storageReport()
    } catch (e) {
      decorate(e)
    }
  }

  async accessStats(scopes, id) {
    try {
      return await this._inner.accessStats(scopes, id)
    } catch (e) {
      decorate(e)
    }
  }

  async rebuildStateFromOps() {
    try {
      return await this._inner.rebuildStateFromOps()
    } catch (e) {
      decorate(e)
    }
  }

  static async openStored(path) {
    try {
      const inner = await native.TopoDB.openStored(path)
      return new TopoDB(inner)
    } catch (e) {
      decorate(e)
    }
  }

  static async openWithOptions(path, indexSpec, cacheSizeBytes) {
    try {
      const inner = await native.TopoDB.openWithOptions(path, indexSpec, cacheSizeBytes)
      return new TopoDB(inner)
    } catch (e) {
      decorate(e)
    }
  }

  async debugDumpNodes() {
    try {
      return await this._inner.debugDumpNodes()
    } catch (e) {
      decorate(e)
    }
  }

  async debugDumpEdges() {
    try {
      return await this._inner.debugDumpEdges()
    } catch (e) {
      decorate(e)
    }
  }

  close() {
    this._inner.close()
  }

  [Symbol.dispose]() {
    this.close()
  }
}

class Subscription {
  constructor(inner) {
    this._inner = inner
  }

  async next(timeoutMs) {
    try {
      return await this._inner.next(timeoutMs)
    } catch (e) {
      decorate(e)
    }
  }

  close() {
    this._inner.close()
  }

  [Symbol.asyncIterator]() {
    return {
      next: async () => {
        const ev = await this.next()
        return ev == null ? { value: undefined, done: true } : { value: ev, done: false }
      },
      return: async () => { this.close(); return { value: undefined, done: true } },
    }
  }
}

module.exports = { TopoDB, Subscription, ops: require('./ops.js') }
