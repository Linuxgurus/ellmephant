//! Interactive REPL (PLAN §6 P6): rustyline, history at `~/.llmphant/history`, tab
//! completion for commands and keys.
//!
//! Commands are parsed by the *same* clap tree as the one-shot CLI ([`Command`]), so the
//! two faces cannot drift; each line is dispatched through [`execute`], which is also what
//! one-shot uses. The model stays warm across commands (the reason v1 has no daemon).

use std::cell::RefCell;
use std::rc::Rc;

use clap::Parser;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Config, Context, Editor, Helper};

use crate::cli::{Command, execute};
use crate::error::{Error, Result};

use super::session::Session;

/// Verbs offered by tab-completion: the REPL's own plus the one-shot tree.
const COMMANDS: &[&str] = &[
    "add", "search", "show", "rm", "tags", "rename", "reembed", "links", "export", "import",
    "help", "quit", "exit",
];

/// A REPL line, parsed with the exact `Command` tree the one-shot CLI uses.
#[derive(Debug, Parser)]
#[command(
    name = "llmphant",
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Line {
    #[command(subcommand)]
    command: Command,
}

/// Tab-completes commands (first word) and memory keys (`show`/`rm`/`rename` arguments).
struct ReplHelper {
    keys: Rc<RefCell<Vec<String>>>,
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        let start = head.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let word = &head[start..];
        let verb = head[..start].split_whitespace().next();
        let candidates = match verb {
            None => candidates(COMMANDS.iter().copied(), word),
            Some("show" | "rm" | "rename") => {
                let keys = self.keys.borrow();
                candidates(keys.iter().map(String::as_str), word)
            }
            Some(_) => Vec::new(),
        };
        Ok((start, candidates))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}
impl Highlighter for ReplHelper {}
impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

/// Prefix-filtered completion pairs.
fn candidates<'a>(items: impl Iterator<Item = &'a str>, word: &str) -> Vec<Pair> {
    items
        .filter(|item| item.starts_with(word))
        .map(|item| Pair {
            display: item.to_string(),
            replacement: item.to_string(),
        })
        .collect()
}

/// Run the REPL until `quit`/`exit`/EOF.
pub fn run(mut session: Session) -> Result<()> {
    session.paths().ensure()?;
    let keys = Rc::new(RefCell::new(session.keys().unwrap_or_default()));

    let mut editor = Editor::<ReplHelper, DefaultHistory>::with_config(Config::default())
        .map_err(|e| Error::Config(format!("repl: {e}")))?;
    editor.set_helper(Some(ReplHelper {
        keys: Rc::clone(&keys),
    }));

    let history = session.paths().history();
    let _ = editor.load_history(&history); // no history yet is fine

    println!(
        "llmphant {} — `help` for commands, `quit` to exit.\nstore: {}",
        crate::VERSION,
        session.paths().root().display()
    );

    loop {
        match editor.readline("llmphant> ") {
            Ok(line) => {
                // Recorded explicitly rather than via `auto_add_history`, which rustyline
                // only applies on a TTY — this keeps history deterministic under a pipe too.
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(trimmed);
                match trimmed {
                    "quit" | "exit" => break,
                    "help" => print_help(),
                    _ => {
                        if let Err(err) = one(&mut session, trimmed) {
                            eprintln!("error: {err}");
                        }
                        *keys.borrow_mut() = session.keys().unwrap_or_default();
                    }
                }
            }
            Err(ReadlineError::Interrupted) => continue, // Ctrl-C abandons the line
            Err(ReadlineError::Eof) => break,            // Ctrl-D exits
            Err(err) => return Err(Error::Config(format!("repl: {err}"))),
        }
    }

    let _ = editor.save_history(&history);
    Ok(())
}

/// Parse and run one REPL line against the live session.
fn one(session: &mut Session, line: &str) -> Result<()> {
    let args = split_args(line)?;
    let parsed = Line::try_parse_from(args).map_err(|e| Error::Config(e.to_string()))?;
    execute(session, parsed.command)
}

/// Split a line into argv, honouring double quotes and backslash escapes.
fn split_args(line: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut started = false;

    for c in line.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            started = true;
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                started = true;
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err(Error::Config("unclosed quote".into()));
    }
    if started {
        args.push(current);
    }
    Ok(args)
}

/// The REPL's cheat sheet.
fn print_help() {
    println!(
        "commands (same flags as the one-shot CLI):\n\
         \x20 add --scope S --title T --body B [--key K] [--type signpost|dictate|fact|profile]\n\
         \x20     [--source SRC] [--tag TAG]...\n\
         \x20 search \"QUERY\" [--scope S] [-k N] [--mode hybrid|vec|fts] [--expand N] [--json]\n\
         \x20 show KEY [--json]\n\
         \x20 rm KEY\n\
         \x20 tags [--json] | tags prune [--dry-run]\n\
         \x20 rename OLD NEW | reembed | links [--json]\n\
         \x20 export PATH | import PATH [--dry-run]\n\
         \x20 help | quit"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_and_plain_args() {
        assert_eq!(
            split_args("search \"roll back argocd\" -k 5").unwrap(),
            vec!["search", "roll back argocd", "-k", "5"]
        );
    }

    #[test]
    fn splits_empty_quoted_arg() {
        assert_eq!(split_args("show \"\"").unwrap(), vec!["show", ""]);
    }

    #[test]
    fn rejects_unclosed_quote() {
        assert!(split_args("search \"oops").is_err());
    }

    #[test]
    fn parses_a_line_through_the_shared_tree() {
        let line =
            Line::try_parse_from(split_args("search \"roll back\" --mode fts").unwrap()).unwrap();
        assert!(matches!(line.command, Command::Search { .. }));
    }
}
