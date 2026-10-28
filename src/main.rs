use std::io::{self, Read, Write};
use std::panic::{self, AssertUnwindSafe};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use dotonce::commands;
use dotonce::config;
use dotonce::fact::{Fact, slug};
use dotonce::facts;
use dotonce::hook;
use dotonce::serve::serve;
use dotonce::store::Store;
use serde_json::json;

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
    /// Run as a Claude Code hook (prompt or session-start). Reads the hook's JSON on stdin and never fails.
    Hook { event: String },
    /// Print the hooks block to add to Claude Code's settings.json.
    HookConfig,
    /// Rule sets in ~/.dotonce/rules: create, check, test, approve and manage them.
    Rules {
        #[command(subcommand)]
        cmd: RulesCmd,
    },
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
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Runs a hook and always returns normally: errors and panics go to
/// errors.log, and Claude Code gets the hook JSON or nothing.
fn run_hook(event: &str) {
    panic::set_hook(Box::new(|info| hook::log_error(&format!("panic: {info}"))));
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
    match Cli::parse().cmd {
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
        Cmd::HookConfig => {
            let exe = std::env::current_exe()?;
            let bin = hook::shell_word(&exe.to_string_lossy());
            let entry = |event: &str| json!([{"hooks": [{"type": "command", "command": format!("{bin} hook {event}"), "timeout": 5}]}]);
            let out = json!({"hooks": {"UserPromptSubmit": entry("prompt"), "SessionStart": entry("session-start")}});
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        Cmd::Rules { cmd } => rules_cmd(cmd),
    }
}
