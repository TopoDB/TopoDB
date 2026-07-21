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
  const spec = { equality: [{ label: 'Entity', prop: 'name' }], text: [] }
  const db = await TopoDB.openWith(path, spec)
  const r = await db.submit(
    [
      ops.createEntity('ada'),
      ops.createMemory('ada wrote the first program'),
      ops.link('#1', '#0', 'ABOUT'),
    ],
    null, NOW,
  )
  return { db, ids: r.ids }
}

test('node and missing', async () => {
  const { db, ids } = await openDb()
  const n = await db.node(S, ids[0])
  assert.equal(n.label, 'Entity')
  assert.equal(n.props.name, 'ada')
  assert.equal(await db.node(S, '01ARZ3NDEKTSV4RRFFQ69G5FAV'), null)
  db.close()
})

test('nodes by label and newest', async () => {
  const { db } = await openDb()
  assert.equal((await db.nodesByLabel(S, 'Entity')).length, 1)
  assert.equal((await db.nodesByLabelNewest(S, 'Memory', 10)).length, 1)
  assert.deepEqual(await db.nodesByLabel(S, 'nope'), [])
  db.close()
})

test('nodes by prop exact normalized and unindexed', async () => {
  const { db, ids } = await openDb()
  const exact = await db.nodesByProp(S, 'Entity', 'name', 'ada')
  assert.equal(exact[0].id, ids[0])
  assert.deepEqual(await db.nodesByProp(S, 'Entity', 'name', 'ADA'), [])
  const normalized = await db.nodesByPropNormalized(S, 'Entity', 'name', '  ADA ')
  assert.equal(normalized[0].id, ids[0])
  await assert.rejects(
    () => db.nodesByProp(S, 'Entity', 'unindexed', 'x'),
    (e) => e.code === 'REJECTED'
  )
  db.close()
})

test('edges', async () => {
  const { db, ids } = await openDb()
  const edges = await db.edgesFrom(S, ids[1])
  assert.equal(edges.length, 1)
  assert.equal(edges[0].type, 'about')
  const allEdges = await db.allEdgesBetween(ids[1], ids[0])
  assert.equal(allEdges.length, 1)
  const openEdges = await db.openEdgesBetween(ids[1], ids[0])
  assert.deepEqual(openEdges, [ids[2]])
  const filtered = await db.edgesFrom(S, ids[1], { type: 'OTHER' })
  assert.deepEqual(filtered, [])
  db.close()
})

test('traverse', async () => {
  const { db, ids } = await openDb()
  const sg = await db.traverse(S, [ids[1]], 2)
  const nodeIds = new Set(sg.nodes.map(n => n.id))
  assert.deepEqual(nodeIds, new Set([ids[0], ids[1]]))
  assert.equal(sg.edges.length, 1)
  await assert.rejects(
    () => db.traverse(S, [ids[1]], 0),
    (e) => e.code === 'REJECTED'
  )
  db.close()
})

test('float range', async () => {
  const { db, ids } = await openDb()
  await db.submit([ops.setNodeProps(ids[0], { score: 0.7 })], null, NOW + 1)
  const result = await db.nodesByFloatRange(S, 'score', 0.0, 1.0)
  assert.equal(result.length, 1)
  db.close()
})
