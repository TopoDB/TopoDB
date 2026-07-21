import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, readdirSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { dirname } from 'node:path'
import { TopoDB } from '../index.js'

// Resolve fixtures directory from ES module context
const __filename = fileURLToPath(import.meta.url)
const __dirname = dirname(__filename)
const fixturesDir = join(__dirname, '../../../fixtures/parity')

// Helper: resolve "#N" back-references to ids
function resolve(x, ids) {
  if (typeof x === 'string' && x.startsWith('#')) {
    return ids[parseInt(x.slice(1))]
  }
  if (Array.isArray(x)) {
    return x.map((v) => resolve(v, ids))
  }
  if (x !== null && typeof x === 'object') {
    const result = {}
    for (const [k, v] of Object.entries(x)) {
      result[k] = resolve(v, ids)
    }
    return result
  }
  return x
}

// Read and sort fixtures
const fixtureFiles = readdirSync(fixturesDir)
  .filter((f) => f.endsWith('.json'))
  .sort()

// Create a test for each fixture
for (const fixtureFile of fixtureFiles) {
  const fixturePath = join(fixturesDir, fixtureFile)
  const fixtureName = fixtureFile.replace('.json', '')

  test(`parity: ${fixtureName}`, async () => {
    const fx = JSON.parse(readFileSync(fixturePath, 'utf8'))
    const spec = fx.index_spec ?? { equality: [], text: [] }

    // Create temp directory for this test
    const tempDir = mkdtempSync(join(tmpdir(), 'topodb-'))
    const dbPath = join(tempDir, 't.redb')

    // Open database with spec
    const db = await TopoDB.openWith(dbPath, spec)

    try {
      // Submit first batch
      const result = await db.submit(fx.batch, null, fx.now_ms)
      const ids = result.ids

      // Handle optional second batch
      if (fx.batch2) {
        const batch2 = resolve(fx.batch2, ids)
        await db.submit(batch2, null, fx.now_ms2)
      }

      // Run checks
      for (const chk of fx.checks) {
        const args = resolve(chk.args, ids)
        const call = chk.call
        let out = null

        if (call === 'node') {
          out = await db.node(args.scopes, args.id)
          assert.equal(out.label, chk.expect_label)
        } else if (call === 'nodes_by_label') {
          out = await db.nodesByLabel(args.scopes, args.label)
          const returnedIds = out.map((n) => n.id)
          const expectedIds = resolve(chk.expect_ids, ids)
          assert.deepEqual(returnedIds, expectedIds)
        } else if (call === 'search_text') {
          out = await db.searchText(args.scopes, args.query, args.k)
          const returnedIds = out.map((h) => h.node.id)
          const expectedIds = resolve(chk.expect_ids, ids)
          assert.deepEqual(returnedIds, expectedIds)
        } else if (call === 'traverse') {
          out = await db.traverse(args.scopes, args.seeds, args.max_hops, args.as_of ? { asOf: args.as_of } : undefined)
          const returnedNodeIds = out.nodes.map((n) => n.id).sort()
          const expectedNodeIds = resolve(chk.expect_node_ids, ids).sort()
          assert.deepEqual(returnedNodeIds, expectedNodeIds)
          if ('expect_edge_count' in chk) {
            assert.equal(out.edges.length, chk.expect_edge_count)
          }
        } else {
          assert.fail(`unknown check call ${JSON.stringify(call)}`)
        }
      }
    } finally {
      db.close()
    }
  })
}
