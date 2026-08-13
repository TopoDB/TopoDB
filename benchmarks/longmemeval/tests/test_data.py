from pathlib import Path
from lme.data import load, dataset_sha256, memory_texts

FIX = Path(__file__).parent / "fixtures" / "tiny_longmemeval.json"


def test_load_parses_questions_sessions_turns():
    qs = load(FIX)
    assert len(qs) == 2
    q1 = qs[0]
    assert q1.id == "q1"
    assert q1.text == "What is my dog's name?"
    assert q1.answer_session_ids == {"sess_a"}
    assert q1.is_abstention is False
    assert len(q1.sessions) == 2
    assert q1.sessions[0].id == "sess_a"
    assert q1.sessions[0].turns[0].content == "My dog is named Rex."


def test_abstention_detected_from_id_suffix():
    qs = load(FIX)
    assert qs[1].is_abstention is True
    assert qs[1].answer_session_ids == set()


def test_memory_texts_session_granularity_is_one_per_session():
    q = load(FIX)[0]
    mems = memory_texts(q, "session")
    assert len(mems) == 2
    sid, content = mems[0]
    assert sid == "sess_a"
    assert "Rex" in content and "great name" in content  # turns joined


def test_memory_texts_turn_granularity_is_one_per_turn():
    q = load(FIX)[0]
    mems = memory_texts(q, "turn")
    assert len(mems) == 3  # 2 + 1 turns
    assert all(sid in {"sess_a", "sess_b"} for sid, _ in mems)


def test_dataset_sha256_is_stable():
    a = dataset_sha256(FIX)
    b = dataset_sha256(FIX)
    assert a == b and len(a) == 64
