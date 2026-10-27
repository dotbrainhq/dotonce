"""Adapter smoke test: no model calls. Run from the runner checkout:
DOTONCE_BIN=/path/to/dotonce/target/release/dotonce uv run python /path/to/dotonce/bench/amb/smoke.py
"""

import json
import tempfile
from pathlib import Path

from memory_bench.memory import get_memory_provider
from memory_bench.models import Document


def main() -> None:
    p = get_memory_provider("dotonce")
    with tempfile.TemporaryDirectory() as tmp:
        p.prepare(Path(tmp), reset=True)
        p.ingest(
            [
                Document(
                    id="q1_s1",
                    user_id="q1",
                    timestamp="2023-05-20T02:21:00+00:00",
                    content=json.dumps([{"role": "user", "content": "I adopted a dog named Rex"}]),
                ),
                Document(
                    id="c1_session_1",
                    user_id="c1",
                    timestamp="2023-05-08T13:56:00+00:00",
                    content=json.dumps([{"speaker": "Caroline", "dia_id": "D1:1", "text": "I went to a support group", "blip_caption": "a poster"}]),
                ),
            ]
        )
        docs, _ = p.retrieve("What is my dog called?", user_id="q1")
        assert [d.id for d in docs] == ["q1_s1"], docs
        assert "Rex" in docs[0].content
        docs, _ = p.retrieve("What did Caroline go to?", user_id="c1")
        assert [d.id for d in docs] == ["c1_session_1"], docs
        assert "poster" in docs[0].content
        p.cleanup()
    print("smoke ok")


if __name__ == "__main__":
    main()
