import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { TopoDB, ops } from '../index.js'

const NOW = 1_700_000_000_000
const dir = () => mkdtempSync(join(tmpdir(), 'topodb-'))

test('opsSince and currentSeq', async () => {
  const db = await TopoDB.open(join(dir(), 't.redb'))
  const r = await db.submit([ops.createEntity('a'), ops.createEntity('b')], null, NOW)
  const currentSeq = await db.currentSeq()
  assert.equal(currentSeq, r.lastSeq)
  const evs = await db.opsSince(0)
  const seqs = evs.map(e => e.seq)
  assert.deepEqual(seqs, [r.firstSeq, r.lastSeq])
  assert(typeof evs[0].op === 'object')
  db.close()
})

test('compactOps then opsSince raises COMPACTED', async () => {
  const db = await TopoDB.open(join(dir(), 't.redb'))
  const r = await db.submit([ops.createEntity('a'), ops.createEntity('b')], null, NOW)
  await db.compactOps(r.lastSeq)
  try {
    await db.opsSince(0)
    assert.fail('expected COMPACTED error')
  } catch (e) {
    assert.equal(e.code, 'COMPACTED')
    assert.equal(e.oldest, r.lastSeq)
  }
  db.close()
})

test('subscribe next timeout before write returns null', async () => {
  const db = await TopoDB.open(join(dir(), 't.redb'))
  const sub = db.subscribe(16)
  const ev = await sub.next(50)
  assert.equal(ev, null)
  db.close()
})

test('subscribe delivers event after submit', async () => {
  const db = await TopoDB.open(join(dir(), 't.redb'))
  const sub = db.subscribe(16)
  const r = await db.submit([ops.createEntity('a')], null, NOW)
  const ev = await sub.next(5000)
  assert.equal(ev.seq, r.firstSeq)
  assert(typeof ev.op === 'object')
  sub.close()
  db.close()
})

test('for await iterates the feed', async () => {
  const db = await TopoDB.open(join(dir(), 't.redb'))
  const sub = db.subscribe(16)
  const got = []
  const consumer = (async () => {
    for await (const ev of sub) {
      got.push(ev)
      break
    }
  })()
  await db.submit([ops.createEntity('a')], null, NOW)
  await consumer
  assert.equal(got.length, 1)
  db.close()
})
