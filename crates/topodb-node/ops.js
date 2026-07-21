// Builders for the batch-submit DSL (shared with topodb-cli / topodb-mcp).
const dropNull = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v != null))

module.exports = {
  createEntity: (name, scope) => dropNull({ op: 'create_entity', name, scope }),
  createMemory: (content, scope) => dropNull({ op: 'create_memory', content, scope }),
  createNode: (label, props, scope) => dropNull({ op: 'create_node', label, props, scope }),
  link: (from, to, type, { props, scope, validFrom } = {}) =>
    dropNull({ op: 'link', from, to, type, props, scope, valid_from: validFrom }),
  // null prop values are deletions — never dropped.
  setNodeProps: (id, props) => ({ op: 'set_node_props', id, props }),
  removeNode: (id) => ({ op: 'remove_node', id }),
  closeEdge: (id, validTo) => dropNull({ op: 'close_edge', id, valid_to: validTo }),
  setEmbedding: (id, model, vector) => ({ op: 'set_embedding', id, model, vector }),
}
