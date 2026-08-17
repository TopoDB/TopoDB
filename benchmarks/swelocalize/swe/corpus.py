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
                content = open(full, "r", encoding="utf-8", errors="replace").read()
            except OSError:
                continue
            out.append((rel, content))
    out.sort(key=lambda pc: pc[0])
    return out

def _window(lines: list, max_lines: int) -> list:
    return ["\n".join(lines[i:i + max_lines]) + "\n"
            for i in range(0, len(lines), max_lines)]

def chunk_file(content: str, max_lines: int = 60) -> list:
    if not content.strip():
        return [content] if content else []
    lines = content.splitlines()
    try:
        tree = ast.parse(content)
    except SyntaxError:
        return _window(lines, max_lines)
    # Group top-level statements into <= max_lines-line chunks.
    bounds = []  # (start_line, end_line) 1-indexed inclusive
    for node in tree.body:
        start = node.lineno
        end = getattr(node, "end_lineno", start)
        bounds.append((start, end))
    if not bounds:
        return _window(lines, max_lines)
    chunks, cur_start, cur_end = [], bounds[0][0], bounds[0][1]
    for start, end in bounds[1:]:
        if end - cur_start + 1 <= max_lines:
            cur_end = end
        else:
            chunks.extend(_emit(lines, cur_start, cur_end, max_lines))
            cur_start, cur_end = start, end
    chunks.extend(_emit(lines, cur_start, cur_end, max_lines))
    return chunks or _window(lines, max_lines)

def _emit(lines: list, start: int, end: int, max_lines: int) -> list:
    seg = lines[start - 1:end]
    if len(seg) <= max_lines:
        return ["\n".join(seg) + "\n"]
    return _window(seg, max_lines)
