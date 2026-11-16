# Changelog

Every release of dotonce is listed here. Versions follow semantic versioning.

## Unreleased

### Added

- `kind = "skill-loaded"`: a check that catches a draft written before its writing skill loaded. It reads the skills Claude called, the slash commands you ran and the `SKILL.md` files Claude read, so a skill loaded early in the session still counts after compaction. At `warn` it logs the catch and your next prompt tells Claude to load the skill first; at `fix` the draft goes back once. Without a `skills` list it uses the installed skills named like writing skills, and with none installed it passes.
- A skill-loaded check needs a `[draft]` table, and `rules new` writes a commented example of one.
- `skip` in the `[draft]` table: patterns for fenced blocks that aren't drafts, like an analysis card in the same bare fence as your replies. A block that one of them matches isn't checked.
- A Claude Code plugin. `claude plugin marketplace add dotbrainhq/dotonce`, then `claude plugin install dotonce@dotonce`, adds all seven hooks, so nothing goes into settings.json by hand. The plugin runs the dotonce binary from your PATH or `~/.cargo/bin`, and tells you at the start of each session when the binary is missing or older than the plugin.
- `dotonce doctor` reads Claude Code's plugin list too. It names the plugin as the source of each hook, says when the plugin is installed but off, and warns when a hook runs twice because it's in the plugin and in a settings file.
- The guard stops Claude from turning the dotonce plugin off: `claude plugin disable` or `uninstall` on dotonce, `disable --all`, and `marketplace remove dotonce`.
- Rule sets turn on by themselves. A new rule file, or a set Claude proposes, starts log-only (`warn`): its checks note what they catch and change nothing. A set that catches the same slips in at least two sessions over its first week moves to `fix` by itself, and one whose corrected replies keep failing moves back to log-only. A level you set with `rules level` stays where you put it. Claude Code tells you about each change once, in a message only you see, with a four-digit code: send `no` and the code to undo it, within a week. Send `dotonce` to see what you can still undo.
- Two kinds of change still wait for your OK: one Claude proposes to a set at `fix`, and one to a set you turned off. Send `yes` and the code to use it, or `no` and the code to drop it. Only hooks that Claude Code runs make changes or take a code: Claude running dotonce from its own shell gets none, and neither do runs without a person, like headless and SDK runs, where sets are approved from a terminal as before. The codes live in `~/.dotonce/chat.json`, which the guard keeps out of Claude's reach like approvals.
- When your prompt reads like a correction of Claude's last reply, a note asks Claude to fix what you asked first, then propose a rule set that would catch the same mistake, if a pattern can. Only Claude sees the note. It never comes on the first prompt of a session, or in a run without a person.
- `dotonce rules propose FILE --ask --note "..."`: a set Claude writes from your correction waits for your yes instead of turning on by itself. The message you see gives its scope, how often it would have caught something in your last 14 days of sessions, the sets already on in the same place, and Claude's one-line note. `yes` and the code turn it on at `fix`, or at the file's own `level`, with a code that undoes it for a week. `no` drops it, and with no answer it goes after 7 days. Replies that still break it after Claude's correction move it back to log-only, like any set at `fix`.
- `dotonce rules propose FILE`: Claude can write a rule set and propose it. Every applied, replaced, undone or dropped file is kept in `~/.dotonce/proposals/done/`.
- A starter from `dotonce rules new` stays off until you edit it, so its examples never run.
- When Claude tries to change a rule file, the guard now tells it to propose the set instead of asking you to run a command.
- The guard stops Claude from running `dotonce rules new`, which left a starter in the rules folder that Claude couldn't change or remove. The reason points it to `dotonce rules propose FILE --ask --note "..."`.
- A Bash command that only reads can name the rules folder and the guarded files: `ls`, `cat`, `head`, `grep`, `wc`, `diff` and a few more readers, with no output to a file. `find` with `-delete`, `-exec` or another action, and `file -C`, don't count as reads.
- The guard reads `>|`, `>&file` and a `$(...)` redirect target as writes to a file.
- An `aliases:` frontmatter list and `dotonce save --aliases`: other names for a fact's topic, matched like its title. The line on how to save a fact asks for at most three short ones and no general words.
- `ignore_paths` in the config: inside those folders the hooks add nothing, check nothing and change nothing. The guard that keeps rule files out of Claude's reach still runs there.
- `dotonce init` and `dotonce doctor` print the line to add to your shell config when dotonce's folder isn't on your PATH. They never edit a shell file.
- Each conversation in the results shows its weekday and how many days before the question it happened, so the agent doesn't have to do date math.
- `bench-serve` accepts `max_sessions` and `fill` per request, and the benchmark adapter passes them from `DOTONCE_MAX_SESSIONS` and `DOTONCE_FILL`.
- Search engine that stores conversation turns exactly as written in SQLite, one index per user, with keyword search (BM25), typo-tolerant search for unknown words, a names list, time windows from words like "two weeks ago", and newest-first ranking for "latest" questions.
- Rank fusion across those lists, sessions scored by their best turns, and packing into a token budget with whole sessions first and focused windows of the matching turns after.
- Speaker names are matched exactly, so a question that names a person finds the turn it asks about, not every turn that person wrote.
- `dotonce bench-serve`, a JSON-lines server that keeps running on bad input.
- Benchmark adapter, installer, smoke test, and comparison script for the agent-memory-benchmark runner.
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

- The prompt hook sends at most three facts, and only those close to the best match. A status line, like "fixed the login redirect, moving on", gets none. A bug report, like "lint failed" or "found a 429 from the API", still gets its facts. A short request on a small store, like "check the logs for this request", now finds its fact. When a prompt pastes a log, code or an email, the typed lines and the pasted text are matched apart, so a pasted 429 response can find the rate limit fact.
- `dotonce hook-config` prints all seven hooks dotonce needs, up from two.
- The default budget is now 40,000 tokens with up to 60 conversations. On LongMemEval this keeps the right conversations for every question in our 102-question sample, while still sending fewer tokens than Hindsight.

### Fixed

- A set's undo code keeps working when a change to the same set starts waiting for your OK. `dotonce` lists both, each with its own code.
- A `yes` or `no` from an earlier message works after the change shows again, in the same session or another. Each change keeps one code until you answer it, it expires or its file changes, and a wrong code leaves the codes on screen as they are.
- A short status message, like "done, next batch", stops pulling in facts that share only common words with it.
- Hooks ignore Claude Code's own messages (task notifications, agent reports, system reminders), so they add no facts or reminders there. Only a prompt that starts with one counts, so a prompt you type that pastes one keeps its facts and reminders. Reply checks still run in those turns.
- A prompt you type keeps its facts and reminders when the desktop app puts a note in front of it, such as a status line about linked sessions. Only the typed words are matched. The app's own events (side-session, CI and cross-session messages) get none, alone or behind a note.
- A rule set's reminder comes once per session, and again after compaction or when its file changes, instead of on every turn.
- Prompt-word triggers read only text the user typed, and a trigger of several words matches only as the whole phrase.
- A hook could panic while writing to a stderr that Claude Code had closed. The hook path never writes to stderr now, even for bad arguments.
- A fact comes once per session, and again after compaction or when the fact changes, instead of with every prompt that matches it. The line on how to save a fact comes once a session too, with the first facts.
- Text pasted in the desktop app counts as pasted, not typed, and a paste brings at most one fact. Common words and short numbers in a paste are left out of the match, so a word like "what" can't pull in a fact whose name starts with it.
- A fact whose name and aliases share no word with the prompt, only its description, comes only when the text search ranks it first.
- A status line like "okay, waiting" gets no facts, and words like "both" and "all" don't count as a topic.
- The session start names a rule set you turned off once, instead of at every session start.
