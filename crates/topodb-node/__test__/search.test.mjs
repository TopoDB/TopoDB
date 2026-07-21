import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { TopoDB, ops } from '../index.js'

const NOW = 1_700_000_000_000
const S = ['shared']

async function openDb() {
  const path = join(mkdtempSync(join(tmpdir(), 'topodb-')), 't.redb')
  const spec = { equality: [], text: [{ label: 'Memory', prop: 'content' }] }
  const db = await TopoDB.openWith(path, spec)
  const r = await db.submit(
    [
      ops.createMemory('ada wrote the first program'),
      ops.createMemory('the analytical engine computes'),
      ops.createEntity('ada'),
      ops.link('#0', '#2', 'ABOUT'),
      ops.setEmbedding('#0', 'toy', [1.0, 0.0]),
      ops.setEmbedding('#1', 'toy', [0.0, 1.0]),
    ],
    null, NOW,
  )
  return { db, ids: r.ids }
}

test('searchText', async () => {
  const { db, ids } = await openDb()
  const hits = await db.searchText(S, 'first program', 5)
  assert.equal(hits[0].node.id, ids[0])
  assert.equal(typeof hits[0].score, 'number')
  await assert.rejects(
    () => db.searchText(S, 'x', 5, { recencyWeight: 2.0 }),
    (e) => e.code === 'REJECTED'
  )
  db.close()
})

test('searchVector', async () => {
  const { db, ids } = await openDb()
  const hits = await db.searchVector(S, 'toy', [1.0, 0.0], 2)
  assert.equal(hits[0].node.id, ids[0])
  await assert.rejects(
    () => db.searchVector(S, 'toy', [], 2),
    (e) => e.code === 'REJECTED'
  )
  const unknown = await db.searchVector(S, 'unknown-model', [1.0, 0.0], 2)
  assert.deepEqual(unknown, [])
  db.close()
})

test('recall text and vector legs', async () => {
  const { db, ids } = await openDb()
  const hits = await db.recall(S, 'first program', 5, {
    vector: { model: 'toy', vector: [1.0, 0.0] },
    nowMs: NOW,
  })
  assert.equal(hits[0].node.id, ids[0])
  await assert.rejects(
    () => db.recall(S, 'x', 5, { labels: [] }),
    (e) => e.code === 'REJECTED'
  )
  db.close()
})

test('suggestLinks', async () => {
  const { db, ids } = await openDb()
  const out = await db.suggestLinks(S, ids[1], 3, { model: 'toy' })
  assert.equal(Array.isArray(out), true)
  for (const s of out) {
    assert.deepEqual(
      new Set(Object.keys(s)),
      new Set(['node', 'score', 'commonNeighbors', 'structural', 'semantic'])
    )
  }
  db.close()
})

test('recall with no opts uses graphBoost=false and still ranks', async () => {
  const { db, ids } = await openDb()
  const hits = await db.recall(S, 'first program', 5)
  assert.equal(hits[0].node.id, ids[0])
  db.close()
})

test('malformed expansions rejects with REJECTED', async () => {
  const { db } = await openDb()
  await assert.rejects(
    () => db.recall(S, 'x', 5, { expansions: 'not-an-array' }),
    (e) => e.code === 'REJECTED'
  )
  db.close()
})
