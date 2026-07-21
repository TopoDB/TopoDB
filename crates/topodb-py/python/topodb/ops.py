"""Builders for the batch-submit DSL (shared with topodb-cli and topodb-mcp).

Each returns a plain command dict; None-valued optional fields are omitted.
"""


def _drop_none(d):
    return {k: v for k, v in d.items() if v is not None}


def create_entity(name, scope=None):
    return _drop_none({"op": "create_entity", "name": name, "scope": scope})


def create_memory(content, scope=None):
    return _drop_none({"op": "create_memory", "content": content, "scope": scope})


def create_node(label, props=None, scope=None):
    return _drop_none({"op": "create_node", "label": label, "props": props, "scope": scope})


def link(from_, to, type, props=None, scope=None, valid_from=None):
    return _drop_none({"op": "link", "from": from_, "to": to, "type": type,
                       "props": props, "scope": scope, "valid_from": valid_from})


def set_node_props(id, props):
    # None values inside `props` are deletions — never dropped.
    return {"op": "set_node_props", "id": id, "props": props}


def remove_node(id):
    return {"op": "remove_node", "id": id}


def close_edge(id, valid_to=None):
    return _drop_none({"op": "close_edge", "id": id, "valid_to": valid_to})


def set_embedding(id, model, vector):
    return {"op": "set_embedding", "id": id, "model": model, "vector": vector}
