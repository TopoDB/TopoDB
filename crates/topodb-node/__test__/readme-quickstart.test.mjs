import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

// The README quickstart is the first code a user runs. This test executes the
// README's own ```js block verbatim (only the db path is redirected into a
// temp dir), so a quickstart that throws or searches an unindexed store goes
// red here instead of in a user's first five minutes. That exact bug shipped
// once: open() + a made-up scope string → REJECTED, and search on the empty
// default IndexSpec → zero hits.
const HERE = dirname(fileURLToPath(import.meta.url))
const readme = readFileSync(join(HERE, '..', 'README.md'), 'utf8')

test('README quickstart runs verbatim and its search finds the memory', async () => {
  // \r?\n + a normalize below: Windows CI checks out with autocrlf, so the
  // fence is ```js\r\n there — the exact mismatch that broke this test once.
  const m = /## Quickstart\s+```js\r?\n([\s\S]*?)```/.exec(readme)
  assert.ok(m, 'README has a ## Quickstart ```js block')
  let code = m[1].replaceAll('\r\n', '\n')
  assert.ok(code.includes("require('topodb')"), 'quickstart requires topodb')
  // Redirect the db file into a temp dir; everything else runs as written.
  const dbPath = join(mkdtempSync(join(tmpdir(), 'readme-qs-')), 'memory.redb')
  code = code.replaceAll("'memory.redb'", JSON.stringify(dbPath))
  // The published package name resolves to this repo checkout in tests.
  code = code.replaceAll("require('topodb')", "require('../index.js')")
  const logged = []
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor
  const fn = new AsyncFunction(
    'require',
    'console',
    code,
  )
  const fakeConsole = { log: (...a) => logged.push(a) }
  const { createRequire } = await import('node:module')
  await fn(createRequire(import.meta.url), fakeConsole)
  // The quickstart prints search hits then the traverse summary. If search
  // found nothing, the hit loop printed nothing and only the traverse line
  // (and any trailing lines) logged.
  assert.ok(logged.length >= 2, `quickstart printed ${logged.length} line(s); its search found nothing`)
  const flat = logged.flat().map((a) => (typeof a === 'object' ? JSON.stringify(a) : String(a))).join(' ')
  assert.ok(/first program/.test(flat), `search hit content not in output: ${flat}`)
})
