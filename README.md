<picture>
  <source media="(prefers-color-scheme: dark)" srcset=".github/assets/cover-dark.png">
  <img src=".github/assets/cover-light.png" alt="dotonce: correct your agent once. Memory and mistake checks for AI coding agents. Local, fast, and open source. Next to the tagline, the music repeat sign with two dots, then the same sign with one dot taken away: it plays once.">
</picture>

# dotonce

Long-term memory and mistake checks for AI coding agents like Claude Code, Codex, and Cursor. Correct your agent once, and it remembers.

## Why dotonce

You tell your agent not to do something. It says it understands. Two sessions later it does it again.

That happens because every session starts from zero. Notes in `CLAUDE.md` or a memory file help, but the agent reads them as suggestions, and a long list of rules gets skipped. The research on this is clear: the more rules an agent carries, and the further they sit from the moment they matter, the more often it ignores them.

dotonce works the other way round. Right before your agent acts, it finds the few facts and rules that matter for what you just asked and hands them over with their sources. Then it checks what the agent does against the rules you set, and stops a mistake you already corrected.

It runs on your machine. There's no server to run, no API key to add, and no AI model to download.

dotonce is free and open source. It's in early development, and there's no release yet.

## What works today

- **Search engine:** stores conversation turns exactly as written in one SQLite file per user, and finds the ones a question needs by keyword, typo-tolerant matching, names, and dates. Results are merged, grouped by conversation, and fitted into a token budget, with each conversation's weekday and how many days before the question it happened.
- **Speed:** with 100,000 stored turns, a search takes about 26 ms, and 95% finish within 31 ms on an Apple M3 Pro.
- **Benchmark adapter:** runs dotonce on LongMemEval and LoCoMo, two of the most used tests for AI agent memory, through the open [agent-memory-benchmark](https://github.com/vectorize-io/agent-memory-benchmark) runner. That puts it side by side with other memory tools, with the same answering model and the same judge.

## What's coming

- **Claude Code hooks:** the right facts and rules arrive before each prompt, and again after the context is compacted.
- **Rules that block:** a rule can stop a tool call, or send a reply back with the exact line that broke it.
- **Your yes first:** a correction becomes a proposed rule, and it only takes effect after you approve it.
- **A repeat-mistake rate:** every catch is logged, so you can see which rules earn their place.
- **More agents:** an MCP server for Codex, Cursor, and any other MCP client.

## How it's built

- **Local only:** dotonce makes no network calls. No telemetry, no auto-update.
- **One small binary:** written in Rust, with SQLite built in. Nothing else to install or keep running.
- **No extra AI cost:** the agent you already use does the reasoning, so dotonce adds no model calls of its own.
- **Facts keep their source:** when something changes, the old fact is marked as replaced, never silently rewritten.

## Build from source

You need Rust 1.85 or later.

```
git clone https://github.com/dotbrainhq/dotonce.git
cd dotonce
cargo build --release
cargo test
```

The speed check is skipped by default. Run it in release mode:

```
cargo test --release --test speed -- --ignored --nocapture
```

## Run the benchmark

The adapter in `bench/amb/` plugs dotonce into a local copy of the agent-memory-benchmark runner. The runner needs Python 3.11, [uv](https://docs.astral.sh/uv/), and an API key for the answering model.

```
python3 bench/amb/install.py /path/to/agent-memory-benchmark
cd /path/to/agent-memory-benchmark
DOTONCE_BIN=/path/to/dotonce/target/release/dotonce uv run python /path/to/dotonce/bench/amb/smoke.py
```

`bench/amb/compare.py` compares result files on the same questions, and stops if the answering model or judge differ. Results go here once the full runs are done.

## Questions

### How is this different from CLAUDE.md or a memory file?

Those files are loaded in full at the start of a session, and the agent decides what to follow. dotonce picks only what fits the task in front of it, shows where each fact came from, and checks the agent's actions against your rules instead of hoping it remembers.

### Does my code or chat history leave my machine?

No. dotonce makes no network calls. Everything stays in a local SQLite file you can open, copy, or delete.

### Do I need an API key?

Not for dotonce itself. The benchmark runner needs one for its answering model, because that's how the test is scored.

### How does it compare with Hindsight, Mem0, or claude-mem?

Many popular memory tools run a background server, or call an AI model every time they save something. dotonce does neither, and it's built to block a repeated mistake, not only remind the agent about it. Benchmark numbers will be published here, run with the same models other tools used.

## Project layout

| Path | What it is |
|---|---|
| `src/` | The search engine and the `dotonce` command. |
| `tests/` | End-to-end and speed tests. |
| `bench/amb/` | The benchmark adapter, installer, smoke test, and comparison script. |
| `brand/` | The mark, icons, and the script that rebuilds them. |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). To report a security issue, see [SECURITY.md](SECURITY.md).

## License

The code is released under the [MIT License](LICENSE). The dotonce name and icon are not covered by the license, so please use your own name and icon if you ship a copy.

Made by Dotbrain.
