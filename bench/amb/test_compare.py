"""Tests for compare.py. Run: python3 bench/amb/test_compare.py"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

COMPARE = Path(__file__).with_name("compare.py")


def result(qid: str, correct: bool, tokens: int = 1000, qtype: str = "single-hop") -> dict:
    return {
        "query_id": qid,
        "correct": correct,
        "context_tokens": tokens,
        "retrieve_time_ms": 5,
        "category_axes": {"Question Type": [qtype]},
    }


def write(path: Path, results: list, answer: str = "a-model", judge: str = "j-model") -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "memory_provider": "dotonce",
                "dataset": "longmemeval",
                "split": "s",
                "answer_llm": answer,
                "judge_llm": judge,
                "results": results,
            }
        )
    )
    return path


def run(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(COMPARE), *args], capture_output=True, text=True
    )


class CompareTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_label_is_run_dir_or_file_name(self):
        a = write(self.tmp / "outputs" / "longmemeval" / "dotonce-v3" / "rag" / "s.json", [result("q1", True)])
        b = write(self.tmp / "baselines" / "hindsight-s.json", [result("q1", False)])
        p = run(str(a), str(b))
        self.assertEqual(p.returncode, 0, p.stderr)
        top, types = p.stdout.split("question type")
        for table in (top, types):
            self.assertIn("dotonce-v3", table)
            self.assertIn("hindsight-s", table)

    def test_compares_only_shared_questions(self):
        a = write(self.tmp / "a.json", [result("q1", True), result("q2", True), result("q3", False)])
        b = write(self.tmp / "b.json", [result("q2", False), result("q3", True), result("q4", True)])
        p = run(str(a), str(b))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("comparing 2 questions", p.stdout)
        self.assertRegex(p.stdout, r"a\s+3 results")
        self.assertRegex(p.stdout, r"b\s+3 results")
        self.assertRegex(p.stdout, r"\n\s*a\s+50\.0%\s+1/2")
        self.assertRegex(p.stdout, r"\n\s*b\s+50\.0%\s+1/2")

    def test_model_mismatch_fails(self):
        a = write(self.tmp / "a.json", [result("q1", True)], answer="m1")
        b = write(self.tmp / "b.json", [result("q1", True)], answer="m2")
        p = run(str(a), str(b))
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("answer_llm", p.stderr)
        c = write(self.tmp / "c.json", [result("q1", True)], judge="j2")
        d = write(self.tmp / "d.json", [result("q1", True)])
        p = run(str(d), str(c))
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("judge_llm", p.stderr)

    def test_context_token_limit(self):
        a = write(self.tmp / "a.json", [result("q1", True, 1000), result("q2", True, 3000)])
        self.assertEqual(run("--max-context-tokens", "2000", str(a)).returncode, 0)
        p = run("--max-context-tokens", "1999", str(a))
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("2000", p.stderr)


if __name__ == "__main__":
    unittest.main()
