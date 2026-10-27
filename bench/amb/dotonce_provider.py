"""dotonce memory provider for the agent-memory-benchmark runner.

install.py copies this file into the runner as memory_bench/memory/dotonce.py.
"""

import json
import os
import subprocess
import threading
from pathlib import Path

from ..models import Document
from .base import MemoryProvider


def _turns(doc: Document) -> list[dict]:
    raw = doc.messages
    if raw is None:
        try:
            raw = json.loads(doc.content)
        except (TypeError, ValueError):
            raw = None
    turns = []
    if isinstance(raw, list):
        for t in raw:
            if not isinstance(t, dict):
                continue
            speaker = t.get("role") or t.get("speaker") or ""
            text = t.get("content") or t.get("text") or ""
            caption = t.get("blip_caption")
            if caption:
                text = f"{text} [shared an image: {caption}]".strip()
            if text:
                turns.append({"speaker": speaker, "text": text})
    if not turns and doc.content:
        turns.append({"speaker": "", "text": doc.content})
    return turns


class DotonceMemoryProvider(MemoryProvider):
    name = "dotonce"
    description = (
        "dotonce: exact turns in SQLite FTS5 (BM25 and trigram), names and time lists, "
        "rank fusion, whole sessions packed in date order. No LLM, no embeddings."
    )
    kind = "local"
    link = "https://github.com/dotbrainhq/dotonce"
    concurrency = 4
    stop_timeout = 30

    def __init__(self):
        self._bin = os.environ.get("DOTONCE_BIN", "dotonce")
        self._budget = int(os.environ.get("DOTONCE_BUDGET_TOKENS", "40000"))
        self._max_sessions = os.environ.get("DOTONCE_MAX_SESSIONS")
        self._fill = os.environ.get("DOTONCE_FILL")
        self._proc = None
        self._lock = threading.Lock()

    def prepare(self, store_dir: Path, unit_ids=None, reset: bool = True) -> None:
        self.cleanup()
        store_dir.mkdir(parents=True, exist_ok=True)
        args = [self._bin, "bench-serve", "--dir", str(store_dir)]
        if reset:
            args.append("--fresh")
        self._proc = subprocess.Popen(
            args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, encoding="utf-8", bufsize=1
        )

    def _call(self, payload: dict) -> dict:
        if self._proc is None:
            raise RuntimeError("dotonce provider used before prepare()")
        with self._lock:
            try:
                self._proc.stdin.write(json.dumps(payload) + "\n")
                self._proc.stdin.flush()
                line = self._proc.stdout.readline()
            except BrokenPipeError:
                line = ""
            if not line:
                raise RuntimeError(f"dotonce bench-serve exited (exit code {self._exit_code()})")
        out = json.loads(line)
        if not out.get("ok"):
            raise RuntimeError(out.get("error", "dotonce error"))
        return out

    def ingest(self, documents: list[Document]) -> None:
        docs = [
            {"id": d.id, "user_id": d.user_id, "timestamp": d.timestamp, "turns": _turns(d)}
            for d in documents
        ]
        for i in range(0, len(docs), 500):
            self._call({"op": "ingest", "docs": docs[i : i + 500]})

    def retrieve(self, query, k=10, user_id=None, query_timestamp=None):
        out = self._call(
            {
                "op": "retrieve",
                "query": query,
                "user_id": user_id,
                "query_timestamp": query_timestamp,
                "budget_tokens": self._budget,
                **({"max_sessions": int(self._max_sessions)} if self._max_sessions else {}),
                **({"fill": self._fill == "1"} if self._fill is not None else {}),
            }
        )
        docs = [
            Document(
                id=s["id"],
                content=s["text"],
                user_id=user_id,
                timestamp=s.get("timestamp"),
                source_ids=[s["id"]],
            )
            for s in out["sessions"]
        ]
        return docs, None

    def _exit_code(self):
        try:
            return self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            return "unknown, still running"

    def cleanup(self) -> None:
        proc, self._proc = self._proc, None
        if proc is None:
            return
        try:
            proc.stdin.close()
        except BrokenPipeError:
            pass
        try:
            proc.wait(timeout=self.stop_timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        finally:
            proc.stdout.close()
