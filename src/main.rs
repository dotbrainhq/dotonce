use std::io::{self, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use dotonce::commands;
use dotonce::config;
use dotonce::fact::{Fact, slug};
use dotonce::facts;
use dotonce::hook;
use dotonce::serve::serve;
use dotonce::store::Store;

#[derive(Parser)]
#[command(
    name = "dotonce",
    version,
    about = "Memory and checks for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// JSON-lines server for the benchmark adapter: one request per stdin line, one reply per stdout line.
    BenchServe {
        /// Folder for the per-user index files, or ":memory:" to keep every index in memory.
        #[arg(long)]
        dir: String,
        #[arg(long)]
        fresh: bool,
    },
    /// Create ~/.dotonce with a config file and a notes folder.
    Init,
    /// Save a fact as a markdown file in a store.
    Save {
        text: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "source")]
        sources: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        #[arg(long)]
        store: Option<String>,
        /// Only show this fact inside a private path. The default when saving from inside one.
        #[arg(long, conflicts_with = "public")]
        private: bool,
        /// Save as public even from inside a private path.
        #[arg(long)]
        public: bool,
        #[arg(long)]
        pinned: bool,
    },
    /// Search saved facts.
    Search {
        query: String,
        #[arg(long)]
        json: bool,
    },
    /// Bring the index up to date with the fact files.
    Index {
        /// Start the index from scratch.
        #[arg(long)]
        rebuild: bool,
    },
    /// Run as a Claude Code hook (prompt, session-start, skill, expansion, stop, pre-tool or display). Reads the hook's JSON on stdin and never fails.
    Hook { event: String },
    /// Print the hooks block to add to Claude Code's settings.json: every hook dotonce needs.
    HookConfig,
    /// Rule sets in ~/.dotonce/rules: create, check, test, approve and manage them.
    Rules {
        #[command(subcommand)]
        cmd: RulesCmd,
    },
    /// Catches per check over 7 and 30 days, fixes, denials and the repeat-mistake rate.
    Stats,
    /// Check that Claude Code's settings have every hook dotonce needs. Reads only.
    Doctor,
}

#[derive(Subcommand)]
enum RulesCmd {
    /// Write a commented starter rule set.
    New {
        id: String,
        /// Check fenced drafts in replies (the default).
        #[arg(long, conflicts_with = "whole_reply")]
        drafts: bool,
        /// Check whole replies instead of fenced drafts.
        #[arg(long)]
        whole_reply: bool,
    },
    /// Check every rule file and list the errors.
    Check,
    /// Run a set's checks on a text file and print what would fail.
    Test {
        id: String,
        file: PathBuf,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
        /// Run this tool's checks instead, reading the file as the tool's text.
        #[arg(long, value_parser = ["Bash", "Write", "Edit", "MultiEdit", "NotebookEdit"])]
        tool: Option<String>,
        /// The file path a Write, Edit, MultiEdit or NotebookEdit call would have.
        #[arg(long, requires = "tool")]
        path: Option<String>,
    },
    /// Approve a set as its file is now. Needs a terminal and the typed id.
    Approve { id: String },
    /// Set an approved set's level: remind, warn or fix. Needs a terminal and the typed id.
    Level { id: String, level: String },
    /// Turn a set off. Needs a terminal and the typed id.
    Off { id: String },
    /// List every set with its level and approval state.
    List,
    /// Show which sets are on here and what turned each one on.
    Why {
        /// The session to show. The newest one when left out.
        #[arg(long)]
        session: Option<String>,
    },
    /// Turn a set off for one session only. Needs a terminal and the typed id.
    Pause {
        id: String,
        /// The session to pause it in. The newest one when left out.
        #[arg(long)]
        session: Option<String>,
    },
    /// Print the most recent catches.
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

fn rules_cmd(cmd: RulesCmd) -> Result<()> {
    match cmd {
        RulesCmd::New {
            id,
            drafts,
            whole_reply,
        } => {
            let whole = if whole_reply {
                Some(true)
            } else if drafts {
                Some(false)
            } else {
                None
            };
            let path = commands::rules_new(&id, whole, &mut io::stdin().lock())?;
            println!("{}", path.display());
            Ok(())
        }
        RulesCmd::Check => commands::rules_check(),
        RulesCmd::Test {
            id,
            file,
            json,
            tool,
            path,
        } => commands::rules_test(&id, &file, json, tool.as_deref(), path.as_deref()),
        RulesCmd::Approve { id } => commands::rules_approve(&id),
        RulesCmd::Level { id, level } => commands::rules_level(&id, &level),
        RulesCmd::Off { id } => commands::rules_off(&id),
        RulesCmd::List => commands::rules_list(),
        RulesCmd::Why { session } => commands::rules_why(session.as_deref()),
        RulesCmd::Pause { id, session } => commands::rules_pause(&id, session.as_deref()),
        RulesCmd::Log { limit } => commands::rules_log(limit),
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Runs a hook and always returns normally: errors and panics go to
/// errors.log, and Claude Code gets the hook JSON or nothing.
fn run_hook(event: &str) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        hook::handle(event, &input, now())
    }));
    match result {
        Ok(Ok(Some(out))) => {
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{out}").and_then(|_| stdout.flush());
        }
        Ok(Ok(None)) => {}
        Ok(Err(e)) => hook::log_error(&format!("hook {event}: {e:#}")),
        Err(_) => {}
    }
}

fn main() -> Result<()> {
    if std::env::args_os().nth(1).is_some_and(|a| a == "hook") {
        hook_main();
        return Ok(());
    }
    run(Cli::parse().cmd)
}

/// The hook path writes nothing to stderr, even for bad arguments: Claude
/// Code may have closed it, and a write to a closed stderr panics.
fn hook_main() {
    panic::set_hook(Box::new(|info| hook::log_error(&format!("panic: {info}"))));
    match Cli::try_parse() {
        Ok(Cli {
            cmd: Cmd::Hook { event },
        }) => run_hook(&event),
        Ok(_) => {}
        Err(e) => hook::log_error(&format!("hook arguments: {e}")),
    }
}

fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::BenchServe { dir, fresh } => {
            let mut st = Store::open(&dir, fresh)?;
            serve(&mut st, io::stdin().lock(), io::stdout().lock())
        }
        Cmd::Init => {
            println!("{}", config::init()?.display());
            Ok(())
        }
        Cmd::Save {
            text,
            name,
            sources,
            tags,
            store,
            private,
            public,
            pinned,
        } => {
            if text.trim().is_empty() {
                bail!("the fact text is empty: give the rule or fact to save");
            }
            if name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                bail!("--name is empty: give a name or leave it out");
            }
            let cfg = config::load()?;
            let cwd = std::env::current_dir().unwrap_or_default();
            let private = private || (!public && cfg.private_allowed(&cwd));
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let f = Fact {
                name: name.unwrap_or_else(|| slug(&text)),
                description: text,
                source: sources,
                tags,
                learned: Some(today.clone()),
                updated: Some(today),
                private,
                pinned,
                ..Fact::default()
            };
            let path = facts::save(&cfg, store.as_deref(), &f)?;
            let mut ix = facts::open()?;
            facts::sync(&mut ix, &cfg)?;
            println!("{}", path.display());
            Ok(())
        }
        Cmd::Search { query, json } => {
            let cfg = config::load()?;
            let mut ix = facts::open()?;
            facts::sync(&mut ix, &cfg)?;
            let cwd = std::env::current_dir().unwrap_or_default();
            let hits = facts::search(&ix, &cfg, &query, &cwd, now(), 4_000)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else {
                for h in &hits {
                    println!("{}", h.line);
                }
            }
            Ok(())
        }
        Cmd::Index { rebuild } => {
            let cfg = config::load()?;
            let mut ix = facts::open()?;
            if rebuild {
                facts::clear(&ix)?;
            }
            let r = facts::sync(&mut ix, &cfg)?;
            println!(
                "added {}, updated {}, removed {}",
                r.added, r.updated, r.removed
            );
            Ok(())
        }
        Cmd::Hook { event } => {
            run_hook(&event);
            Ok(())
        }
        Cmd::HookConfig => commands::hook_config(),
        Cmd::Rules { cmd } => rules_cmd(cmd),
        Cmd::Stats => commands::stats(),
        Cmd::Doctor => commands::doctor(),
    }
}
