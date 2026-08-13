"""Render a results JSON (from run.py) as a human-readable table."""
import argparse
import json
from pathlib import Path


LABEL_W = 32
COL_W = 10


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
    return "\n".join(lines)


def main(argv=None) -> None:
    p = argparse.ArgumentParser(description="Render a LongMemEval-S results JSON")
    p.add_argument("results_json")
    args = p.parse_args(argv)
    print(render(json.loads(Path(args.results_json).read_text())))


if __name__ == "__main__":
    main()
