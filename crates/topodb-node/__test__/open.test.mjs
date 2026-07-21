import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { TopoDB } from '../index.js'

const dir = () => mkdtempSync(join(tmpdir(), 'topodb-'))

test('open and formatVersion', async () => {
  const db = await TopoDB.open(join(dir(), 't.redb'))
  assert.equal(typeof (await db.formatVersion()), 'number')
  db.close()
})

test('use after close rejects with CLOSED', async () => {
  const db = await TopoDB.open(join(dir(), 't.redb'))
  db.close()
  await assert.rejects(() => db.formatVersion(), (e) => e.code === 'CLOSED')
})

test('open bad path rejects with STORAGE', async () => {
  await assert.rejects(
    () => TopoDB.open('/nonexistent-dir-xyz/t.redb'),
    (e) => e.code === 'STORAGE',
  )
})
