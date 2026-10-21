"""Install the dotonce provider into a local agent-memory-benchmark checkout.

Usage: python3 bench/amb/install.py ~/Documents/claude-work/dotonce/amb
Safe to run twice.
"""

import shutil
import sys
from pathlib import Path

IMPORT_LINE = "from .dotonce import DotonceMemoryProvider\n"
REGISTRY_LINE = 'REGISTRY["dotonce"] = DotonceMemoryProvider\n'


def main() -> None:
    runner = Path(sys.argv[1]).expanduser()
    memory_dir = runner / "src" / "memory_bench" / "memory"
    init = memory_dir / "__init__.py"
    if not init.exists():
        sys.exit(f"not a runner checkout: {init} missing")
    shutil.copyfile(Path(__file__).with_name("dotonce_provider.py"), memory_dir / "dotonce.py")
    text = init.read_text()
    if IMPORT_LINE not in text:
        text = text.replace("from .none import NoMemoryProvider\n", "from .none import NoMemoryProvider\n" + IMPORT_LINE, 1)
    if REGISTRY_LINE not in text:
        text = text.replace("\n\ndef get_memory_provider", "\n" + REGISTRY_LINE + "\n\ndef get_memory_provider", 1)
    if IMPORT_LINE not in text or REGISTRY_LINE not in text:
        sys.exit("runner layout changed: could not patch memory/__init__.py")
    init.write_text(text)
    print(f"installed dotonce provider into {memory_dir}")


if __name__ == "__main__":
    main()
