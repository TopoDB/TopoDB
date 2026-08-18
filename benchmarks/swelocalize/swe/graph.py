"""Best-effort file-level import graph via the stdlib ast module.

Resolution is deliberately conservative: an edge exists only when an import
resolves to a file that is actually in the corpus. Dynamic imports, re-exports
through __init__, star-imports, and conditional imports may be missed — these
limits are documented in README.md, not hidden."""
import ast

def _module_to_candidates(dotted: str) -> list:
    """A dotted module name -> the relpaths that could define it."""
    parts = dotted.split(".")
    return ["/".join(parts) + ".py", "/".join(parts) + "/__init__.py"]

def _relative_base(importer: str, level: int) -> list:
    """Package parts to prepend for a `level`-dots relative import."""
    pkg = importer.split("/")[:-1]            # drop the filename
    if level <= 1:
        return pkg
    trimmed = pkg[: len(pkg) - (level - 1)]
    return trimmed

def build_import_graph(files: list) -> dict:
    file_set = {rel for rel, _ in files}
    graph = {rel: set() for rel, _ in files}
    for rel, content in files:
        try:
            tree = ast.parse(content)
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            targets = []
            if isinstance(node, ast.Import):
                for alias in node.names:
                    targets.extend(_module_to_candidates(alias.name))
            elif isinstance(node, ast.ImportFrom):
                if node.level and node.level > 0:
                    base = _relative_base(rel, node.level)
                    mod = (base + (node.module.split(".") if node.module else []))
                    dotted = ".".join(mod)
                    is_relative = True
                else:
                    dotted = node.module or ""
                    is_relative = False
                if dotted:
                    # Bare `from . import X` (no module): `dotted` is the package
                    # dir itself, so linking its __init__.py is spurious noise —
                    # skip it and rely on the named-target candidates below. But
                    # `from .util import X` (module present) must still link the
                    # `.util` module, so only skip when node.module is None.
                    if not (is_relative and node.module is None):
                        targets.extend(_module_to_candidates(dotted))
                    # `from pkg import name` may target pkg/name.py too.
                    for alias in node.names:
                        targets.extend(_module_to_candidates(dotted + "." + alias.name))
            for cand in targets:
                if cand in file_set and cand != rel:
                    graph[rel].add(cand)
    return graph
