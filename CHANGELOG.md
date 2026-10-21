# Changelog

Every release of dotonce is listed here. Versions follow semantic versioning.

## Unreleased

### Added

- Search engine that stores conversation turns exactly as written in SQLite, one index per user, with keyword search (BM25), typo-tolerant search for unknown words, a names list, time windows from words like "two weeks ago", and newest-first ranking for "latest" questions.
- Rank fusion across those lists, sessions scored by their best turns, and packing into a token budget with whole sessions first and focused windows of the matching turns after.
- Speaker names are matched exactly, so a question that names a person finds the turn it asks about, not every turn that person wrote.
- `dotonce bench-serve`, a JSON-lines server that keeps running on bad input.
- Benchmark adapter, installer, smoke test, and comparison script for the agent-memory-benchmark runner.
- The dotonce mark: the music repeat sign with one dot taken away, so it plays once.
