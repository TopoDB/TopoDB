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

  async formatVersion() {
    try {
      return await this._inner.formatVersion()
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

module.exports = { TopoDB }
