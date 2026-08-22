import re
from pathlib import Path

CRATE_DIR = Path(__file__).resolve().parent.parent


def _toml_version(path, section):
    """Version under the given [section], ignoring dependency tables."""
    current = ""
    for line in path.read_text().splitlines():
        header = re.match(r"\s*\[([^\]]+)\]", line)
        if header:
            current = header.group(1)
        kv = re.match(r'\s*version\s*=\s*"([^"]+)"', line)
        if kv and current == section:
            return kv.group(1)
    raise AssertionError(f"no [{section}] version in {path}")


def _installed_version():
    try:
        from importlib.metadata import version
        return version("topodb")
    except Exception:
        import topodb
        return topodb.__version__


def test_installed_version_matches_pyproject():
    expected = _toml_version(CRATE_DIR / "pyproject.toml", "project")
    assert _installed_version() == expected


def test_installed_version_matches_cargo():
    expected = _toml_version(CRATE_DIR / "Cargo.toml", "package")
    assert _installed_version() == expected


def test_pyproject_matches_cargo():
    assert _toml_version(CRATE_DIR / "pyproject.toml", "project") == \
        _toml_version(CRATE_DIR / "Cargo.toml", "package")
