"""Turn a checked-out repo into indexable documents."""
import ast
import os

def iter_py_files(root: str) -> list:
    out = []
    for dirpath, _dirs, files in os.walk(root):
        for name in files:
            if not name.endswith(".py"):
                continue
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root).replace(os.sep, "/")
            try:
                with open(full, "r", encoding="utf-8", errors="replace") as fh:
                    content = fh.read()
            except OSError:
                continue
            out.append((rel, content))
    out.sort(key=lambda pc: pc[0])
    return out

def chunk_file(content: str, max_lines: int = 60) -> list:
    """Split file text into <= max_lines-line chunks, breaking at top-level ast
    statement boundaries. Coverage is COMPLETE: every character of `content`
    (shebangs, license headers, comments, and blank lines between or after
    statements) lands in exactly one chunk — nothing is dropped, so the embedded
    corpus indexes the whole file. Falls back to fixed line windows when the file
    does not parse or a single top-level node exceeds max_lines."""
    if not content:
        return []
    if not content.strip():
        return [content]
    lines = content.splitlines(keepends=True)
    total = len(lines)

    def window(seg: list) -> list:
        return ["".join(seg[i:i + max_lines]) for i in range(0, len(seg), max_lines)]

    try:
        tree = ast.parse(content)
    except SyntaxError:
        return window(lines)
    cuts = sorted({getattr(n, "end_lineno", n.lineno) for n in tree.body})
    if not cuts:
        return window(lines)

    chunks: list = []
    start = 1   # 1-indexed next unconsumed line
    last = 0    # last cut that fit in the current chunk
    for cut in cuts:
        if cut - start + 1 > max_lines and last >= start:
            chunks.extend(window(lines[start - 1:last]))
            start = last + 1
        last = cut
    if start <= total:
        chunks.extend(window(lines[start - 1:total]))
    return chunks or [content]
