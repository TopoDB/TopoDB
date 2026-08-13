def test_topodb_binding_importable():
    import topodb
    assert hasattr(topodb, "TopoDB")


def test_ops_module_importable():
    from topodb import ops
    assert callable(ops.create_memory)
    assert callable(ops.set_embedding)
