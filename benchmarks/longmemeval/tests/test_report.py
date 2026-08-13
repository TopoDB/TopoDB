from lme.report import render


def test_render_includes_manifest_and_a_row_per_config():
    results = {
        "manifest": {
            "model_tag": "toy", "ks": [1, 3], "granularities": ["session"],
            "legs": ["text", "vector"], "n_questions": 2, "n_graded": 1,
            "dataset_sha256": "abc123", "depth": 100, "limit": None, "seed": 0,
        },
        "results": {
            "session:text": {"n": 1, "n_abstention": 1, "overall": {"recall@1": 0.0, "recall@3": 1.0, "coverage@1": 0.0, "coverage@3": 1.0}, "per_type": {}},
            "session:vector": {"n": 1, "n_abstention": 1, "overall": {"recall@1": 1.0, "recall@3": 1.0, "coverage@1": 1.0, "coverage@3": 1.0}, "per_type": {}},
        },
    }
    txt = render(results)
    assert "toy" in txt              # manifest echoed
    assert "session:text" in txt     # a row per config
    assert "session:vector" in txt
    assert "recall@1" in txt
    assert "1.000" in txt            # formatted number present


def test_header_and_data_columns_align():
    results = {
        "manifest": {
            "model_tag": "toy", "ks": [1, 3], "granularities": ["session"],
            "legs": ["text"], "n_questions": 2, "n_graded": 1,
            "dataset_sha256": "abcdef012345", "depth": 100, "limit": None, "seed": 0,
        },
        "results": {
            "session:text": {"n": 1, "n_abstention": 1,
                             "overall": {"recall@1": 0.0, "recall@3": 1.0,
                                         "coverage@1": 0.0, "coverage@3": 1.0},
                             "per_type": {}},
        },
    }
    txt = render(results)
    lines = txt.splitlines()
    header = next(l for l in lines if "recall@1" in l)
    datarow = next(l for l in lines if "session:text" in l)
    # The value column starts at the same offset in the header and the data row.
    assert header.index("recall@1") == datarow.index("0.000")
    # And the second column too.
    assert header.index("recall@3") == datarow.rindex("1.000")
