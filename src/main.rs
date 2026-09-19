//! `jcr` — a TUI code-review tool for Jujutsu, callable by agents.
//!
//! Opens a review over the diff `base..head`, collects comments, suggested
//! edits and deletions, and prints them to stdout on submit (plain text, or
//! JSON with `--json`). Nothing else goes to stdout, so the caller can consume
//! it directly.
//!
//! Exit codes: `0` submitted, `1` quit without submitting, `2` usage or
//! runtime error.

mod diff;
mod input;
mod jj;
mod model;
mod output;
mod tui;

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

const USAGE: &str = "\
jcr — review a Jujutsu diff in a TUI

USAGE:
    jcr [REVSET] [--json] [--help] [--version]

ARGS:
    REVSET    Base revision of the review; the diff is shown as
              `REVSET..head`. Defaults to the revision before head.

OPTIONS:
    --json        Print the submitted review as JSON instead of plain text.
    -h, --help    Show this help.
    -V, --version Show the version.

RESOLUTION:
    head is `@`, unless `@` is both empty and undescribed, in which case head
    is `@-` — so a bare `jcr` right after `jj commit` reviews the change you
    just finished.

KEYS:
    Tab / Shift+Tab  cycle Diff · Files · Comments
    j k ↑ ↓          move               ] [      next / previous hunk
    n p              next / prev file   g G      first / last
    v                visual selection   Enter    open file / goto comment
    c                comment            e        suggested edit
    x                mark for deletion  d        clear / delete annotation
    ?                help               s        submit
    q / Esc / Ctrl-C quit without submitting

EXIT CODES:
    0  review submitted (an empty review counts)
    1  quit without submitting
    2  usage or runtime error (nothing is written to stdout)
";

/// Parsed command line.
#[derive(Debug, PartialEq, Eq)]
struct Args {
    revset: Option<String>,
    json: bool,
}

enum Parsed {
    Run(Args),
    Help,
    Version,
}

/// Hand-rolled argument parsing: two options and one positional.
fn parse_args(argv: &[String]) -> Result<Parsed, String> {
    let mut revset: Option<String> = None;
    let mut json = false;
    let mut only_positional = false;

    for arg in argv {
        if only_positional {
            set_revset(&mut revset, arg)?;
            continue;
        }
        match arg.as_str() {
            "--" => only_positional = true,
            "--json" => json = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option `{other}`"));
            }
            other => set_revset(&mut revset, other)?,
        }
    }
    Ok(Parsed::Run(Args { revset, json }))
}

fn set_revset(slot: &mut Option<String>, value: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("unexpected extra argument `{value}`"));
    }
    *slot = Some(value.to_string());
    Ok(())
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("jcr {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(args)) => args,
        Err(msg) => {
            eprintln!("jcr: {msg}");
            eprintln!("Try `jcr --help`.");
            return ExitCode::from(2);
        }
    };

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("jcr: needs an interactive terminal (run it in a TTY)");
        return ExitCode::from(2);
    }

    match review(&args) {
        Ok(Some(text)) => {
            // Printed after the alternate screen is gone, so it lands in the
            // caller's scrollback and pipes cleanly.
            let mut out = io::stdout().lock();
            if out.write_all(text.as_bytes()).is_err() || out.flush().is_err() {
                return ExitCode::from(2);
            }
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::from(1),
        Err(err) => {
            eprintln!("jcr: {err}");
            ExitCode::from(2)
        }
    }
}

/// Runs the review. `Ok(None)` means the user quit without submitting.
fn review(args: &Args) -> Result<Option<String>, String> {
    let target = jj::resolve(args.revset.as_deref()).map_err(|e| e.to_string())?;
    let raw =
        jj::diff(&target.base.commit_id, &target.head.commit_id).map_err(|e| e.to_string())?;
    let files = diff::parse(&raw);
    let empty_state = files.is_empty().then(|| {
        format!(
            "No changes in {}..{}",
            target.base.short(),
            target.head.short()
        )
    });

    let mut app = tui::App::new(target, files, empty_state);
    // `ratatui::init` installs a panic hook that restores the terminal, which
    // is what we want: a panic inside the TUI must not leave raw mode on.
    let mut terminal = ratatui::init();
    let outcome = tui::run(&mut terminal, &mut app);
    ratatui::restore();
    if !outcome.map_err(|e| format!("terminal error: {e}"))? {
        return Ok(None);
    }

    let mut annotations = app.take_annotations();
    let rendered = if args.json {
        output::render_json(app.target(), app.files(), &mut annotations)
    } else {
        output::render_text(app.target(), app.files(), &mut annotations)
    };
    Ok(Some(rendered))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Parsed, String> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        parse_args(&argv)
    }

    fn run_args(argv: &[&str]) -> Args {
        match parse(argv) {
            Ok(Parsed::Run(args)) => args,
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn bare_invocation_has_no_revset() {
        assert_eq!(
            run_args(&[]),
            Args {
                revset: None,
                json: false
            }
        );
    }

    #[test]
    fn positional_revset_is_kept_verbatim() {
        assert_eq!(run_args(&["main"]).revset.as_deref(), Some("main"));
        assert_eq!(run_args(&["main..@-"]).revset.as_deref(), Some("main..@-"));
    }

    #[test]
    fn json_flag_is_recognised_anywhere() {
        assert!(run_args(&["--json"]).json);
        assert!(run_args(&["main", "--json"]).json);
        assert_eq!(
            run_args(&["main", "--json"]).revset.as_deref(),
            Some("main")
        );
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(parse(&["--help"]), Ok(Parsed::Help)));
        assert!(matches!(parse(&["-h"]), Ok(Parsed::Help)));
        assert!(matches!(parse(&["--version"]), Ok(Parsed::Version)));
        assert!(matches!(parse(&["-V"]), Ok(Parsed::Version)));
        assert!(matches!(parse(&["main", "--help"]), Ok(Parsed::Help)));
    }

    #[test]
    fn double_dash_stops_option_parsing() {
        let args = run_args(&["--", "--json"]);
        assert_eq!(args.revset.as_deref(), Some("--json"));
        assert!(!args.json);
    }

    #[test]
    fn unknown_options_and_extra_args_are_errors() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
    }

    #[test]
    fn single_dash_is_a_revset_not_an_option() {
        assert_eq!(run_args(&["-"]).revset.as_deref(), Some("-"));
    }
}
