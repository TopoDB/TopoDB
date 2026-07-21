import pytest
import topodb


def test_open_and_format_version(tmp_path):
    db = topodb.TopoDB.open(str(tmp_path / "t.redb"))
    assert isinstance(db.format_version(), int)
    db.close()


def test_use_after_close_raises_closed(tmp_path):
    db = topodb.TopoDB.open(str(tmp_path / "t.redb"))
    db.close()
    with pytest.raises(topodb.ClosedError):
        db.format_version()


def test_context_manager_closes(tmp_path):
    with topodb.TopoDB.open(str(tmp_path / "t.redb")) as db:
        db.format_version()
    with pytest.raises(topodb.ClosedError):
        db.format_version()


def test_open_bad_path_raises_storage():
    with pytest.raises(topodb.StorageError):
        topodb.TopoDB.open("/nonexistent-dir-xyz/t.redb")


def test_error_hierarchy():
    for name in ("StorageError", "EncodingError", "RejectedError",
                 "CompactedError", "ClosedError", "UnsupportedFormatError"):
        assert issubclass(getattr(topodb, name), topodb.TopoDBError)
