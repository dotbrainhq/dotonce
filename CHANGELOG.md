# Changelog

Every release of dotonce is listed here. Versions follow semantic versioning.

## Unreleased

### Added

- Each conversation in the results shows its weekday and how many days before the question it happened, so the agent doesn't have to do date math.
- `bench-serve` accepts `max_sessions` and `fill` per request, and the benchmark adapter passes them from `DOTONCE_MAX_SESSIONS` and `DOTONCE_FILL`.

### Changed

- The default budget is now 40,000 tokens with up to 60 conversations. On LongMemEval this keeps the right conversations for every question in our 102-question sample, while still sending fewer tokens than Hindsight.

- Search engine that stores conversation turns exactly as written in SQLite, one index per user, with keyword search (BM25), typo-tolerant search for unknown words, a names list, time windows from words like "two weeks ago", and newest-first ranking for "latest" questions.
- Rank fusion across those lists, sessions scored by their best turns, and packing into a token budget with whole sessions first and focused windows of the matching turns after.
- Speaker names are matched exactly, so a question that names a person finds the turn it asks about, not every turn that person wrote.
- `dotonce bench-serve`, a JSON-lines server that keeps running on bad input.
- Benchmark adapter, installer, smoke test, and comparison script for the agent-memory-benchmark runner.
- The dotonce mark: the music repeat sign with one dot taken away, so it plays once.
