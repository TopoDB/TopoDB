#!/usr/bin/env python3
"""Cross-surface parity: run fixtures/parity/*.json through topodb-cli.

Usage: python3 scripts/parity_cli.py [--cli "cargo run -q -p topodb-cli --"]
Exits 1 on any mismatch. SKIPs (never silently) checks the CLI can't express.
"""
import argparse
import json
import pathlib
import shlex
import subprocess
import sys
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from parity_lib import resolve  # noqa: E402

ROOT = pathlib.Path(__file__).parents[1]
FIXTURES = sorted((ROOT / "fixtures" / "parity").glob("*.json"))


def run_cli(cli, db, *args):
    """Run a CLI command and return parsed JSON output or None."""
    cmd = [*cli, "--db", str(db), *args]
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)
        if not result.stdout.strip():
            return None
        return json.loads(result.stdout)
    except subprocess.CalledProcessError as e:
        print(f"FAIL CLI command failed: {' '.join(cmd)}")
        print(f"  stderr: {e.stderr}")
        return None
    except json.JSONDecodeError as e:
        print(f"FAIL CLI output not valid JSON: {e}")
        print(f"  stdout: {result.stdout}")
        return None


def run_fixture(fixture_path, cli):
    """Run all checks in a fixture. Return (ok_count, skip_count, fail_count)."""
    fixture_name = fixture_path.stem
    fx = json.loads(fixture_path.read_text())

    ok_count = 0
    skip_count = 0
    fail_count = 0

    # Create temp db for this fixture
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = pathlib.Path(tmpdir) / "test.redb"

        # Write batch to temp file
        batch_file = pathlib.Path(tmpdir) / "batch.json"
        batch_file.write_text(json.dumps(fx["batch"]))

        # Submit first batch and capture ids
        submit_result = run_cli(cli, db_path, "submit", str(batch_file))
        if not submit_result or "ids" not in submit_result:
            print(f"FAIL {fixture_name} submit (no ids in response)")
            return (0, 0, 1)

        ids = [str(id_val) if id_val is not None else None for id_val in submit_result["ids"]]

        # Handle optional second batch
        if "batch2" in fx:
            batch2 = resolve(fx["batch2"], ids)
            batch2_file = pathlib.Path(tmpdir) / "batch2.json"
            batch2_file.write_text(json.dumps(batch2))
            submit_result2 = run_cli(cli, db_path, "submit", str(batch2_file))
            if not submit_result2:
                print(f"FAIL {fixture_name} submit batch2")
                return (0, 0, 1)
            # batch2 ids are also available, but fixture checks only use #0..#N from batch
            ids.extend([str(id_val) if id_val is not None else None for id_val in submit_result2["ids"]])

        # Run checks
        for chk in fx["checks"]:
            call = chk["call"]
            args = resolve(chk["args"], ids)

            if call == "node":
                # Get the node by id and check label
                node_id = args["id"]
                result = run_cli(cli, db_path, "get", node_id)
                if not result or not result.get("found"):
                    print(f"FAIL {fixture_name} node #{ids.index(node_id)} (not found)")
                    fail_count += 1
                    continue

                node = result["node"]
                expected_label = chk["expect_label"]
                if node.get("label") == expected_label:
                    print(f"OK {fixture_name} node #{ids.index(node_id)}")
                    ok_count += 1
                else:
                    print(f"FAIL {fixture_name} node #{ids.index(node_id)} label")
                    print(f"  expected: {expected_label}, got: {node.get('label')}")
                    fail_count += 1

            elif call == "nodes_by_label":
                # CLI find requires property filtering, nodes_by_label needs label-only
                print(f"SKIP {fixture_name} nodes_by_label (CLI find requires --prop/--value filters)")
                skip_count += 1

            elif call == "search_text":
                query = args["query"]
                k = args.get("k", 10)
                result = run_cli(cli, db_path, "search", query, "--k", str(k))
                if result is None:
                    result = []

                # Extract ids from search results
                actual_ids = [h["node"]["id"] for h in result]
                expected_ids = resolve(chk["expect_ids"], ids)

                if actual_ids == expected_ids:
                    print(f"OK {fixture_name} search_text")
                    ok_count += 1
                else:
                    print(f"FAIL {fixture_name} search_text")
                    print(f"  expected: {expected_ids}, got: {actual_ids}")
                    fail_count += 1

            elif call == "traverse":
                # Check if as_of is present
                if args.get("as_of") is not None:
                    print(f"SKIP {fixture_name} traverse (CLI writes use wall clock, can't test as_of)")
                    skip_count += 1
                    continue

                seed = args["seeds"][0]  # traverse takes a single seed
                max_hops = args.get("max_hops", 2)
                result = run_cli(cli, db_path, "traverse", seed, "--max-hops", str(max_hops))
                if not result or "subgraph" not in result:
                    print(f"FAIL {fixture_name} traverse (no subgraph in response)")
                    fail_count += 1
                    continue

                subgraph = result["subgraph"]
                actual_node_ids = sorted([n["id"] for n in subgraph.get("nodes", [])])
                expected_node_ids = sorted(resolve(chk["expect_node_ids"], ids))

                node_ids_match = actual_node_ids == expected_node_ids
                edge_count_match = True
                if "expect_edge_count" in chk:
                    actual_edge_count = len(subgraph.get("edges", []))
                    expected_edge_count = chk["expect_edge_count"]
                    edge_count_match = actual_edge_count == expected_edge_count
                    if not edge_count_match:
                        print(f"FAIL {fixture_name} traverse edge_count")
                        print(f"  expected: {expected_edge_count}, got: {actual_edge_count}")
                        fail_count += 1
                        continue

                if node_ids_match:
                    print(f"OK {fixture_name} traverse")
                    ok_count += 1
                else:
                    print(f"FAIL {fixture_name} traverse node_ids")
                    print(f"  expected: {expected_node_ids}, got: {actual_node_ids}")
                    fail_count += 1

            else:
                print(f"FAIL {fixture_name} unknown check call {call!r}")
                fail_count += 1

    return (ok_count, skip_count, fail_count)


def main():
    parser = argparse.ArgumentParser(description="CLI parity runner over parity fixtures")
    parser.add_argument(
        "--cli",
        default="cargo run -q -p topodb-cli --",
        help="CLI invocation (will be split with shlex)",
    )
    args = parser.parse_args()

    cli = shlex.split(args.cli)

    # Build CLI once
    print("Building topodb-cli...")
    build_result = subprocess.run(
        ["cargo", "build", "-p", "topodb-cli"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if build_result.returncode != 0:
        print(f"FAIL: cargo build failed")
        print(build_result.stderr)
        sys.exit(1)

    total_ok = 0
    total_skip = 0
    total_fail = 0

    for fixture_path in FIXTURES:
        ok, skip, fail = run_fixture(fixture_path, cli)
        total_ok += ok
        total_skip += skip
        total_fail += fail

    print(f"\nSummary: {total_ok} OK, {total_skip} SKIP, {total_fail} FAIL")

    if total_fail > 0:
        sys.exit(1)
    sys.exit(0)


if __name__ == "__main__":
    main()
