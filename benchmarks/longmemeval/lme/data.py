"""Load and shape the LongMemEval-S dataset."""
import hashlib
import json
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Turn:
    role: str
    content: str


@dataclass
class Session:
    id: str
    date: str | None
    turns: list[Turn]


@dataclass
class Question:
    id: str
    type: str
    text: str
    answer_session_ids: set[str]
    sessions: list[Session]

    @property
    def is_abstention(self) -> bool:
        # LongMemEval marks abstention questions with an "_abs" question_id suffix.
        return "_abs" in self.id


def load(path) -> list[Question]:
    raw = json.loads(Path(path).read_text())
    questions: list[Question] = []
    for item in raw:
        sids = item.get("haystack_session_ids", [])
        dates = item.get("haystack_dates", [])
        raw_sessions = item.get("haystack_sessions", [])
        sessions = []
        for i, turns in enumerate(raw_sessions):
            sessions.append(
                Session(
                    id=sids[i],
                    date=dates[i] if i < len(dates) else None,
                    turns=[Turn(t.get("role", ""), t.get("content", "")) for t in turns],
                )
            )
        questions.append(
            Question(
                id=item["question_id"],
                type=item.get("question_type", "unknown"),
                text=item["question"],
                answer_session_ids=set(item.get("answer_session_ids", [])),
                sessions=sessions,
            )
        )
    return questions


def _session_text(s: Session) -> str:
    return "\n".join(f"{t.role}: {t.content}" for t in s.turns)


def memory_texts(q: Question, granularity: str) -> list[tuple[str, str]]:
    """(session_id, content) pairs to ingest as memories."""
    return [(sid, content) for sid, _date, content in memory_records(q, granularity)]


def memory_records(q: Question, granularity: str) -> list[tuple[str, str | None, str]]:
    """(session_id, date, content) triples to ingest as memories.

    ``date`` is the raw per-session haystack date string (or None when the
    dataset omits it); downstream ingest parses it to Unix ms and leaves
    ``valid_from`` open when it is missing or unparseable (spec §5.3, §4).
    """
    out: list[tuple[str, str | None, str]] = []
    if granularity == "session":
        for s in q.sessions:
            out.append((s.id, s.date, _session_text(s)))
    elif granularity == "turn":
        for s in q.sessions:
            for t in s.turns:
                out.append((s.id, s.date, f"{t.role}: {t.content}"))
    else:
        raise ValueError(f"unknown granularity: {granularity!r}")
    return out


def dataset_sha256(path) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def download(dest) -> None:
    """Best-effort pointer to the dataset; manual fetch may be required."""
    raise SystemExit(
        "LongMemEval-S must be downloaded manually. Get longmemeval_s.json from "
        "the official LongMemEval release (see https://github.com/xiaowu0162/LongMemEval) "
        f"and place it at {dest}."
    )
