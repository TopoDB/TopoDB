"""Render a results JSON (from run.py) as a human-readable table."""
import argparse
import json
from pathlib import Path


def render(results: dict) -> str:
    m = results["manifest"]
    lines: list[str] = []
    lines.append("LongMemEval-S recall")
    lines.append(
        f"  model={m['model_tag']} depth={m['depth']} "
        f"questions={m['n_questions']} graded={m['n_graded']} "
        f"seed={m['seed']} data={m['dataset_sha256'][:12]}"
    )
    ks = m["ks"]
    header = "  {:<22}".format("config") + "".join(f"recall@{k:<5}" for k in ks)
    lines.append(header)
    lines.append("  " + "-" * (len(header) - 2))
    for cfg_name, block in results["results"].items():
        row = "  {:<22}".format(cfg_name)
        row += "".join("{:<6.3f}".format(block["overall"][f"recall@{k}"]) for k in ks)
        lines.append(row)
        for qtype, tblock in sorted(block.get("per_type", {}).items()):
            trow = "    {:<20}".format(f"· {qtype}")
            trow += "".join("{:<6.3f}".format(tblock[f"recall@{k}"]) for k in ks)
            lines.append(trow)
    return "\n".join(lines)


def main(argv=None) -> None:
    p = argparse.ArgumentParser(description="Render a LongMemEval-S results JSON")
    p.add_argument("results_json")
    args = p.parse_args(argv)
    print(render(json.loads(Path(args.results_json).read_text())))


if __name__ == "__main__":
    main()
