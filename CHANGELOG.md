# Changelog

Every release of dotonce is listed here. Versions follow semantic versioning.

## Unreleased

### Added

- `ignore_paths` in the config: inside those folders the hooks add nothing, check nothing and change nothing. The guard that keeps rule files out of Claude's reach still runs there.
- `dotonce init` and `dotonce doctor` print the line to add to your shell config when dotonce's folder isn't on your PATH. They never edit a shell file.
- Each conversation in the results shows its weekday and how many days before the question it happened, so the agent doesn't have to do date math.
- `bench-serve` accepts `max_sessions` and `fill` per request, and the benchmark adapter passes them from `DOTONCE_MAX_SESSIONS` and `DOTONCE_FILL`.
- Search engine that stores conversation turns exactly as written in SQLite, one index per user, with keyword search (BM25), typo-tolerant search for unknown words, a names list, time windows from words like "two weeks ago", and newest-first ranking for "latest" questions.
- Rank fusion across those lists, sessions scored by their best turns, and packing into a token budget with whole sessions first and focused windows of the matching turns after.
- Speaker names are matched exactly, so a question that names a person finds the turn it asks about, not every turn that person wrote.
- `dotonce bench-serve`, a JSON-lines server that keeps running on bad input.
- Benchmark adapter, installer, smoke test, and comparison script for the agent-memory-benchmark runner.
- The dotonce mark: the music repeat sign with one dot taken away, so it plays once.
- Markdown fact files with frontmatter, in one or more store folders. An existing folder of notes, like a knowledge base, works as a store without copying.
- `dotonce init`, `save`, `search`, `index`, and `hook-config`, with the index kept in sync with the files on every call.
- Claude Code hooks: `dotonce hook prompt` adds the matching facts before each prompt, and `dotonce hook session-start` adds pinned facts at the start of a session and after compaction. A failing hook adds nothing and never stops Claude Code. Private facts only show inside the folders you list in the config, and facts saved from inside those folders are private by default.
- Rule sets: TOML files in `~/.dotonce/rules/` that turn on by skill, folder or prompt words, add their reminder at that moment, and share the context cap with facts. Parsing is strict, and a file with an error is skipped whole and reported at the next session start.
- Reply checks at Stop: phrases, patterns, required phrases, bare URLs, repeated names, and paragraph and word limits, on fenced drafts or whole replies. A `fix` set sends a broken draft back once with the line and the reason, and a line right after the broken draft says a corrected one follows. The draft itself is never hidden.
- Tool checks before Bash, Write, Edit, MultiEdit and NotebookEdit calls. A `fix` set denies a failing call with the reason.
- Approvals tied to each file's hash, with levels `remind`, `warn` and `fix`. New sets start at `warn`. Approving, changing a level, turning a set off and pausing it need a real terminal and the typed id, and refuse to run when `CLAUDECODE` or `CLAUDE_CODE_ENTRYPOINT` is set.
- An always-on guard that stops Claude from editing rule files and approval state, and from running the approval commands. Every change shows in `dotonce rules list` and in a notice at the next session start.
- A catch log in `~/.dotonce/catches.jsonl`, with private sets in their own file, and `dotonce stats` with catches per check, fixes, denials and the repeat-mistake rate.
- `dotonce rules new`, `check`, `test`, `approve`, `level`, `off`, `pause`, `list`, `why` and `log`.
- `dotonce doctor`, which checks that the user and project settings have every hook and warns about a `DOTONCE_HOME` set in them. It never edits them.

### Changed

- `dotonce hook-config` prints all seven hooks dotonce needs, up from two.
- The default budget is now 40,000 tokens with up to 60 conversations. On LongMemEval this keeps the right conversations for every question in our 102-question sample, while still sending fewer tokens than Hindsight.

### Fixed

- A short status message, like "done, next batch", no longer pulls in facts that share only common words with it. A short prompt that asks nothing now needs a fact that covers most of it.
- Hooks ignore Claude Code's own messages (task notifications, agent reports, system reminders), so they add no facts or reminders there. Only a prompt that starts with one counts, so a prompt you type that pastes one keeps its facts and reminders. Reply checks still run in those turns.
- A rule set's reminder comes once per session, and again after compaction or when its file changes, instead of on every turn.
- Prompt-word triggers read only text the user typed, and a trigger of several words matches only as the whole phrase.
- A hook could panic while writing to a stderr that Claude Code had closed. The hook path never writes to stderr now, even for bad arguments.
