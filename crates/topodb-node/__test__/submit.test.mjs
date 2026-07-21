import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { TopoDB, ops } from '../index.js'

const NOW = 1_700_000_000_000
const open = () => TopoDB.open(join(mkdtempSync(join(tmpdir(), 'topodb-')), 't.redb'))

test('submit batch with backrefs', async () => {
  const db = await open()
  const r = await db.submit(
    [ops.createEntity('ada'), ops.createMemory('ada wrote the first program'), ops.link('#1', '#0', 'ABOUT')],
    null, NOW,
  )
  assert.deepEqual(Object.keys(r).sort(), ['firstSeq', 'ids', 'lastSeq'])
  assert.equal(r.lastSeq - r.firstSeq, 2)
  assert.equal(r.ids.length, 3)
  db.close()
})

test('bad batch rejects with REJECTED', async () => {
  const db = await open()
  await assert.rejects(() => db.submit([{ op: 'no_such_op' }]), (e) => e.code === 'REJECTED')
  await assert.rejects(() => db.submit({ not: 'an array' }), (e) => e.code === 'REJECTED')
  db.close()
})

test('ops builders shapes', () => {
  assert.deepEqual(ops.createEntity('ada'), { op: 'create_entity', name: 'ada' })
  assert.deepEqual(ops.link('#1', '#0', 'ABOUT'), { op: 'link', from: '#1', to: '#0', type: 'ABOUT' })
})
