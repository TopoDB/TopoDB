"""Render a results JSON (from run.py) as a human-readable table."""
import argparse
import json
from pathlib import Path


LABEL_W = 32
COL_W = 10

# Widths for the graph on/off delta table (§5.5): wider columns so the
# 'graph on'/'graph off'/'delta' markers fit without truncation.
DELTA_COL_W = 14
# ks the delta table reports (spec §5.5: hybrid R@1 and R@3).
DELTA_KS = [1, 3]
# Question types called out in the spec; surfaced first, then the rest.
DELTA_PRIORITY_TYPES = ["multi-session", "temporal-reasoning"]
# Substrings in a leg name that mark a hybrid config as graph-off.
GRAPH_OFF_MARKERS = ("nograph", "no-graph", "graph-off", "graphoff", "graph_off")


def _leg_of(cfg_name: str) -> str:
    return cfg_name.split(":", 1)[1] if ":" in cfg_name else cfg_name


def _granularity_of(cfg_name: str) -> str:
    return cfg_name.split(":", 1)[0] if ":" in cfg_name else ""


def _is_hybrid(leg: str) -> bool:
    return "hybrid" in leg


def _is_graph_off(leg: str) -> bool:
    return any(marker in leg for marker in GRAPH_OFF_MARKERS)


def _ordered_types(*blocks: dict) -> list[str]:
    """Union of per-type keys across the given blocks, priority types first."""
    seen: set[str] = set()
    for block in blocks:
        seen.update(block.get("per_type", {}).keys())
    head = [t for t in DELTA_PRIORITY_TYPES if t in seen]
    tail = sorted(t for t in seen if t not in DELTA_PRIORITY_TYPES)
    return head + tail


def render_graph_delta(results: dict) -> list[str]:
    """Per-question-type table of hybrid R@1/R@3 for graph-on vs graph-off,
    with the delta (graph-on minus graph-off). Reuses the per-type recall
    aggregation already computed by run.py (block['per_type'][qtype]['recall@k'])
    rather than recomputing anything."""
    ks = [k for k in DELTA_KS if k in results["manifest"].get("ks", DELTA_KS)]
    if not ks:
        return []

    # Pair the hybrid graph-on and graph-off blocks within each granularity.
    by_gran: dict[str, dict[str, dict]] = {}
    for cfg_name, block in results["results"].items():
        leg = _leg_of(cfg_name)
        if not _is_hybrid(leg):
            continue
        slot = by_gran.setdefault(_granularity_of(cfg_name), {"on": None, "off": None})
        key = "off" if _is_graph_off(leg) else "on"
        if slot[key] is None:
            slot[key] = block

    lines: list[str] = []
    lines.append("")
    lines.append("hybrid recall by question type: graph on vs graph off")

    on_markers = [f"graph on R@{k}" for k in ks]
    off_markers = [f"graph off R@{k}" for k in ks]
    delta_markers = [f"delta R@{k}" for k in ks]
    header = (
        "  " + "type".ljust(LABEL_W)
        + "".join(m.ljust(DELTA_COL_W) for m in on_markers + off_markers + delta_markers)
    )

    rendered_any = False
    for gran in sorted(by_gran):
        pair = by_gran[gran]
        on_block, off_block = pair["on"], pair["off"]
        if on_block is None or off_block is None:
            continue
        rendered_any = True
        lines.append(f"  granularity={gran or '(none)'}")
        lines.append(header)
        lines.append("  " + "-" * (LABEL_W + DELTA_COL_W * 3 * len(ks)))
        on_pt = on_block.get("per_type", {})
        off_pt = off_block.get("per_type", {})
        for qtype in _ordered_types(on_block, off_block):
            on_t = on_pt.get(qtype, {})
            off_t = off_pt.get(qtype, {})
            cells: list[str] = []
            for k in ks:
                cells.append(_fmt(on_t.get(f"recall@{k}")))
            for k in ks:
                cells.append(_fmt(off_t.get(f"recall@{k}")))
            for k in ks:
                cells.append(_fmt_delta(on_t.get(f"recall@{k}"), off_t.get(f"recall@{k}")))
            lines.append("  " + qtype.ljust(LABEL_W) + "".join(c.ljust(DELTA_COL_W) for c in cells))

    if not rendered_any:
        lines.append(header)
        lines.append("  " + "-" * (LABEL_W + DELTA_COL_W * 3 * len(ks)))
        lines.append("  (no paired graph-on / graph-off hybrid configs in results)")

    return lines


def _fmt(v) -> str:
    return f"{v:.3f}" if isinstance(v, (int, float)) else "-"


def _fmt_delta(on, off) -> str:
    if isinstance(on, (int, float)) and isinstance(off, (int, float)):
        return f"{on - off:+.3f}"
    return "-"


def render(results: dict) -> str:
    m = results["manifest"]
    ks = m["ks"]
    lines: list[str] = []
    lines.append("LongMemEval-S recall")
    lines.append(
        f"  model={m['model_tag']} depth={m['depth']} "
        f"questions={m['n_questions']} graded={m['n_graded']} "
        f"seed={m['seed']} data={m['dataset_sha256'][:12]}"
    )
    header = "  " + "config".ljust(LABEL_W) + "".join(f"recall@{k}".ljust(COL_W) for k in ks)
    lines.append(header)
    lines.append("  " + "-" * (LABEL_W + COL_W * len(ks)))
    for cfg_name, block in results["results"].items():
        row = "  " + cfg_name.ljust(LABEL_W) + "".join(
            f"{block['overall'][f'recall@{k}']:.3f}".ljust(COL_W) for k in ks
        )
        lines.append(row)
        for qtype, tblock in sorted(block.get("per_type", {}).items()):
            label = f"· {qtype}"
            trow = "    " + label.ljust(LABEL_W - 2) + "".join(
                f"{tblock[f'recall@{k}']:.3f}".ljust(COL_W) for k in ks
            )
            lines.append(trow)
    lines.extend(render_graph_delta(results))
    return "\n".join(lines)


def main(argv=None) -> None:
    p = argparse.ArgumentParser(description="Render a LongMemEval-S results JSON")
    p.add_argument("results_json")
    args = p.parse_args(argv)
    print(render(json.loads(Path(args.results_json).read_text())))


if __name__ == "__main__":
    main()
