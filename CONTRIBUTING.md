# Contributing

Every change follows the same path, even a one-word fix.

1. Open an issue with a type label (`bug`, `enhancement` or `documentation`), an area label and a milestone.
2. Branch from an up-to-date `main`: `fix/<issue>-<slug>`, `feat/<issue>-<slug>`, `docs/<issue>-<slug>` or `chore/<slug>`.
3. Keep commits small, with an imperative subject: `Add`, `Fix`, `Update`, `Remove`, `Refactor`, `Test` or `Docs`.
4. Open a pull request that says `Closes #<issue>`.
5. Squash-merge once the `CI passed` check is green.

## Checks

Run these before opening a pull request:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 bench/amb/test_compare.py
```

If you changed search or storage, also run the speed check in release mode and include its p50 and p95 lines in the pull request:

```
cargo test --release --test speed -- --ignored --nocapture
```

## Ground rules

- dotonce makes no network calls and ships no AI model. A change that adds either won't be merged.
- A change to search needs numbers: the speed check, and the benchmark result before and after if it changes what gets found.
- If you changed the mark or icons, edit `brand/scripts/make_brand.py` and rebuild with it, rather than editing the SVGs by hand.
