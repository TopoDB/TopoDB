import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { TopoDB, ops } from '../index.js'

const NOW = 1_700_000_000_000
const S = ['shared']
const dir = () => mkdtempSync(join(tmpdir(), 'topodb-'))

async function waitForStats(db, scopes, nodeId, deadlineS = 5.0) {
  const end = Date.now() + deadlineS * 1000
  while (Date.now() < end) {
    const st = await db.accessStats(scopes, nodeId)
    if (st !== null && st.accessCount >= 1) {
      return st
    }
    await new Promise(r => setTimeout(r, 10))
  }
  throw new Error('access count never became positive within deadline')
}

test('admin surface', async () => {
  const spec = { equality: [{ label: 'Entity', prop: 'name' }], text: [] }
  const db = await TopoDB.openWith(join(dir(), 't.redb'), spec)
  const r = await db.submit([ops.createEntity('ada')], null, NOW)

  const indexSpec = await db.indexSpec()
  assert.deepEqual(indexSpec, spec)

  const report = await db.storageReport()
  assert(Array.isArray(report) && report.length > 0)

  // Node exists but never accessed: stats exist with accessCount=0
  const st0 = await db.accessStats(S, r.ids[0])
  assert.notEqual(st0, null)
  assert.equal(st0.accessCount, 0)

  // Absent node: returns null
  const stAbsent = await db.accessStats(S, '01ARZ3NDEKTSV4RRFFQ69G5FAV')
  assert.equal(stAbsent, null)

  // Bump stats by reading
  await db.nodesByLabel(S, 'Entity')
  const st = await waitForStats(db, S, r.ids[0])
  assert(st.accessCount >= 1)

  await db.rebuildStateFromOps()
  const node = await db.node(S, r.ids[0])
  assert.notEqual(node, null)

  const nodes = await db.debugDumpNodes()
  assert.equal(nodes.length, 1)

  const edges = await db.debugDumpEdges()
  assert.deepEqual(edges, [])

  db.close()
})

test('openStored reopens', async () => {
  const p = join(dir(), 't.redb')
  const db = await TopoDB.open(p)
  await db.submit([ops.createEntity('ada')], null, NOW)
  db.close()

  const db2 = await TopoDB.openStored(p)
  const nodes = await db2.nodesByLabel(S, 'Entity')
  assert.equal(nodes.length, 1)
  db2.close()
})

test('openWithOptions with cache size', async () => {
  const spec = { equality: [], text: [] }
  const db = await TopoDB.openWithOptions(join(dir(), 't.redb'), spec, 1_000_000)
  const v = await db.formatVersion()
  assert(typeof v === 'number')
  db.close()
})
