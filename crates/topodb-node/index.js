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

  close() {
    this._inner.close()
  }

  [Symbol.dispose]() {
    this.close()
  }
}

module.exports = { TopoDB, ops: require('./ops.js') }
