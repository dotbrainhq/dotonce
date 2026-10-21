"""Adapter process-handling tests: no model calls. Install the adapter first, then
run from the runner checkout:
DOTONCE_BIN=~/DotbrainLLC/dotonce/target/release/dotonce uv run python ~/DotbrainLLC/dotonce/bench/amb/test_provider.py
"""

import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from memory_bench.memory import get_memory_provider

ROUND_TRIP = """
import json, sys, tempfile
from pathlib import Path
from memory_bench.memory import get_memory_provider
from memory_bench.models import Document
p = get_memory_provider("dotonce")
with tempfile.TemporaryDirectory() as tmp:
    p.prepare(Path(tmp), reset=True)
    turns = [{"role": "user", "content": "Zo\\u00eb loves cr\\u00e8me br\\u00fbl\\u00e9e"}]
    p.ingest([Document(id="s1", user_id="u1", content=json.dumps(turns))])
    docs, _ = p.retrieve("cr\\u00e8me", user_id="u1")
    p.cleanup()
print(json.dumps([d.content for d in docs]))
"""


def fake_bin(dir: Path, name: str, body: str) -> str:
    path = dir / name
    path.write_text("#!/bin/sh\n" + body + "\n")
    path.chmod(0o755)
    return str(path)


class ProviderTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_non_ascii_text_survives_an_ascii_locale(self):
        env = dict(os.environ, LC_ALL="C", LANG="C", PYTHONCOERCECLOCALE="0", PYTHONUTF8="0")
        out = subprocess.run([sys.executable, "-c", ROUND_TRIP], env=env, capture_output=True, text=True)
        self.assertEqual(out.returncode, 0, out.stderr[-2000:])
        self.assertIn("cr\\u00e8me br\\u00fbl\\u00e9e", out.stdout)

    def test_dead_server_error_names_the_exit_code(self):
        p = get_memory_provider("dotonce")
        p._bin = fake_bin(self.tmp, "dies", "exit 3")
        p.prepare(self.tmp / "store", reset=True)
        time.sleep(0.2)
        with self.assertRaises(RuntimeError) as ctx:
            p.retrieve("anything", user_id="u1")
        self.assertIn("exit code 3", str(ctx.exception))
        p.cleanup()
        self.assertIsNone(p._proc)

    def test_cleanup_kills_a_server_that_does_not_stop(self):
        p = get_memory_provider("dotonce")
        p._bin = fake_bin(self.tmp, "stuck", "exec sleep 600 2>/dev/null")
        p.stop_timeout = 0.5
        p.prepare(self.tmp / "store", reset=True)
        proc = p._proc
        p.cleanup()
        self.assertIsNone(p._proc)
        self.assertIsNotNone(proc.poll())


if __name__ == "__main__":
    unittest.main()
