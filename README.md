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

- **Your facts, as plain files:** every fact is a markdown file with its source and date. Point dotonce at a folder you already keep notes in, and it reads those files as they are.
- **Context before each prompt:** a Claude Code hook finds the few facts that match what you just asked and adds them, with their dates and files, before Claude answers. Pinned facts are added at the start of every session and again after the context is compacted.
- **Private facts stay private:** a fact marked private only shows when Claude Code runs inside a folder you list as private. Facts you save from inside one of those folders are private by default.
- **Search engine:** keyword search, typo-tolerant matching, names, and dates, merged and fitted into a token budget. With 100,000 stored turns a search takes about 26 ms, and 95% finish within 31 ms on an Apple M3 Pro. The prompt hook runs in about 20 ms on a store of 550 facts.
- **Benchmark adapter:** runs dotonce on LongMemEval and LoCoMo, two of the most used tests for AI agent memory, through the open [agent-memory-benchmark](https://github.com/vectorize-io/agent-memory-benchmark) runner. That puts it side by side with other memory tools, with the same answering model and the same judge.

## What's coming

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

## Use it with Claude Code

Install the binary, then create the config and a notes folder in `~/.dotonce`:

```
cargo install --path .
dotonce init
```

Save a fact and find it again:

```
dotonce save "The staging site is staging.example.com" --source "team wiki"
dotonce search "staging site"
```

Print the hooks block, then add its two entries to the `hooks` section of `~/.claude/settings.json`:

```
dotonce hook-config
```

From then on, each prompt gets the facts that match it. The hooks never stop Claude Code: if something goes wrong, they add nothing and write the error to `~/.dotonce/errors.log`.

### Use a folder you already have

Any folder of markdown files with frontmatter works as a store. Add a `[[stores]]` block at the end of `~/.dotonce/config.toml`:

```
[[stores]]
name = "kb"
path = "~/notes"
```

dotonce reads the files in place and never changes them. Facts with `privacy: private` in their frontmatter only show when Claude Code runs inside a folder listed in `private_paths`. Edit the `private_paths = []` line near the top of the file:

```
private_paths = ["~/projects/personal"]
```

Keep `private_paths` above the first `[[stores]]` block. Below it, TOML reads the line as part of that store, and dotonce refuses the config with an error that names the key.

Run `dotonce index` to check the store. It prints how many facts were added, updated, and removed.

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
