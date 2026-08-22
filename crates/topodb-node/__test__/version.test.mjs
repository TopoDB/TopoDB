import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const read = (rel) => readFileSync(new URL(rel, import.meta.url), 'utf8')

// Version under [package], ignoring version keys in dependency tables.
const cargoVersion = (toml) => {
  let section = ''
  for (const line of toml.split('\n')) {
    const header = line.match(/^\s*\[([^\]]+)\]/)
    if (header) section = header[1]
    const kv = line.match(/^\s*version\s*=\s*"([^"]+)"/)
    if (kv && section === 'package') return kv[1]
  }
  throw new Error('no [package] version in Cargo.toml')
}

test('package.json version matches Cargo.toml (pin guard)', () => {
  const pkg = JSON.parse(read('../package.json'))
  assert.equal(pkg.version, cargoVersion(read('../Cargo.toml')))
})
