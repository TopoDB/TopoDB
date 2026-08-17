"""Render the per-leg x k results as an aligned table."""

def format_table(results: dict, ks: list) -> str:
    cols = ["leg"] + [f"any@{k}" for k in ks] + ["mrr"]
    widths = [max(len(cols[0]), max((len(leg) for leg in results), default=0))]
    widths += [max(len(c), 6) for c in cols[1:]]
    def row(cells):
        return "  ".join(str(c).ljust(w) for c, w in zip(cells, widths))
    lines = [row(cols)]
    for leg in sorted(results):
        r = results[leg]
        cells = [leg] + [f"{r.get(f'any@{k}', 0.0):.3f}" for k in ks] \
                      + [f"{r.get('mrr', 0.0):.3f}"]
        lines.append(row(cells))
    return "\n".join(lines)
