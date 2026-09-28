//! Shelling out to `jj`. No jj library crates: the CLI is version-robust and
//! jj has to be in `PATH` for the tool to be useful anyway.
//!
//! All commands run non-interactively (`--no-pager --color never`) and their
//! stdout is captured. Revsets are passed as direct argv entries, so there is
//! no shell involved and no quoting to get wrong.

use std::io;
use std::process::Command;

/// The resolved review target: the diff `base..head`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The revset the user asked for, or `head + "-"` when it was omitted.
    pub revset: String,
    pub base: Rev,
    pub head: Rev,
}

/// Identity of one end of the diff, for the submit header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rev {
    /// Full commit id, as reported by `jj log`.
    pub commit_id: String,
    /// First line of the description; empty when the change is undescribed.
    pub description: String,
}

impl Rev {
    /// Commit id truncated the way the submit header shows it.
    pub fn short(&self) -> &str {
        &self.commit_id[..self.commit_id.len().min(12)]
    }

    pub fn short_description(&self) -> &str {
        self.description.lines().next().unwrap_or("").trim()
    }
}

/// Runs `jj <args>` in `dir`, with the same non-interactive flags as [`jj`].
///
/// Reviewing a checkout that is not the process's CWD is only used by tests,
/// so this stays `cfg(test)` rather than widening the public surface.
#[cfg(test)]
fn jj_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("jj")
        .current_dir(dir)
        .args(["--no-pager", "--color", "never"])
        .args(args)
        .output()
        .expect("jj should be installed for the test suite")
}

#[derive(Debug)]
pub struct JjError(pub String);

impl std::fmt::Display for JjError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JjError {}

type Result<T> = std::result::Result<T, JjError>;

/// Runs `jj <args>` and returns stdout. A non-zero exit is an error carrying
/// jj's stderr, which is what the user needs to see.
fn jj(args: &[&str]) -> Result<String> {
    let out = Command::new("jj")
        .args(["--no-pager", "--color", "never"])
        .args(args)
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => JjError("jj not found in PATH".into()),
            _ => JjError(format!("failed to run jj: {e}")),
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.trim();
        return Err(JjError(if msg.is_empty() {
            format!("jj exited with {}", out.status)
        } else {
            msg.to_string()
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolves the review target from an optional user-supplied revset.
///
/// `head` is `@-` when `@` is both empty and undescribed (the "smart shift"),
/// otherwise `@`. `base` is the user's revset, or `head` minus one revision.
///
/// When the user passes a range like `main..@`, jj unions everything it
/// matches, which is not what they mean: the review is that range's oldest
/// endpoint up to its newest, so both endpoints are resolved here.
pub fn resolve(revset: Option<&str>) -> Result<Target> {
    let head_revset = if is_empty()? && description("@")?.trim().is_empty() {
        "@-"
    } else {
        "@"
    };

    let (base, head) = match revset {
        Some(r) if r.contains("..") => {
            // `A..B` is already a review range, so use both endpoints rather
            // than the range as a base and `@` as the head.
            match r.split_once("..") {
                Some((a, b)) if !a.trim().is_empty() && !b.trim().is_empty() => {
                    (a.trim().to_string(), b.trim().to_string())
                }
                _ => (r.to_string(), head_revset.to_string()),
            }
        }
        Some(r) if is_set_expression(r) => {
            // A union/intersection: the review spans its members.
            let members = rev_list(r)?;
            let base = members.first().cloned().unwrap_or_else(|| r.to_string());
            let head = members
                .last()
                .cloned()
                .unwrap_or_else(|| head_revset.to_string());
            (base, head)
        }
        Some(r) => (r.to_string(), head_revset.to_string()),
        None => (format!("{head_revset}-"), head_revset.to_string()),
    };

    Ok(Target {
        revset: revset.map(str::to_string).unwrap_or_else(|| base.clone()),
        base: rev(&base)?,
        head: rev(&head)?,
    })
}

/// True for revsets that can match more than one commit.
fn is_set_expression(revset: &str) -> bool {
    revset.contains("::") || revset.contains('|') || revset.contains('&')
}

/// Commit ids a revset matches, oldest first.
fn rev_list(revset: &str) -> Result<Vec<String>> {
    let ids = jj(&[
        "log",
        "-r",
        revset,
        "--no-graph",
        "-T",
        "commit_id ++ \"\\n\"",
    ])?;
    Ok(ids
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// True when the given revision has no diff against its parents.
pub fn is_empty() -> Result<bool> {
    Ok(diff_summary("@")?.trim().is_empty())
}

/// `jj diff -r <rev> --summary`, used only as an emptiness probe.
pub fn diff_summary(revset: &str) -> Result<String> {
    jj(&["diff", "-r", revset, "--summary"])
}

/// The description of a revision, verbatim (may be multi-line).
pub fn description(revset: &str) -> Result<String> {
    jj(&["log", "-r", revset, "--no-graph", "-T", "description"])
}

/// The full git-style diff between two revsets.
pub fn diff(from: &str, to: &str) -> Result<String> {
    jj(&["diff", "--git", "--from", from, "--to", to])
}

fn rev(revset: &str) -> Result<Rev> {
    // `commit_id` and `description` are queried separately because
    // descriptions are free-form and may contain any separator we could pick.
    // A multi-revision revset matches several commits; `jj log` prints them
    // oldest first, so the head is the *last* line and the base the first.
    let ids = jj(&[
        "log",
        "-r",
        revset,
        "--no-graph",
        "-T",
        "commit_id ++ \"\\n\"",
    ])?;
    let mut lines: Vec<&str> = ids
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return Err(JjError(format!("no revision matches `{revset}`")));
    }
    // A set expression matches several commits; the head is the newest.
    let commit_id = if is_set_expression(revset) || revset.contains("..") {
        lines.last().unwrap().to_string()
    } else {
        lines.remove(0).to_string()
    };
    Ok(Rev {
        commit_id,
        description: description(revset)?.trim_end().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_truncates_to_twelve() {
        let r = Rev {
            commit_id: "42d29bfe9fd6abc123".into(),
            description: "initial setup".into(),
        };
        assert_eq!(r.short(), "42d29bfe9fd6");
        assert_eq!(r.short_description(), "initial setup");
    }

    #[test]
    fn short_handles_empty_id() {
        let r = Rev::default();
        assert_eq!(r.short(), "");
        assert_eq!(r.short_description(), "");
    }

    #[test]
    fn short_description_takes_first_line() {
        let r = Rev {
            commit_id: "abc".into(),
            description: "one\n\ntwo".into(),
        };
        assert_eq!(r.short_description(), "one");
    }

    /// Creates a throwaway jj repo described as `described`, and returns its
    /// path. The path is leaked on purpose: tests need it to outlive the guard
    /// for the process's lifetime, and the OS temp dir cleans it up.
    fn scratch_repo(described: bool) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jcr-jj-tests-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch repo");
        let init = jj_in(&dir, &["git", "init"]);
        assert!(init.status.success(), "jj git init failed: {init:?}");
        // A fresh repo has no `main` bookmark; `resolve(Some("main"))` needs one.
        let bookmark = jj_in(&dir, &["bookmark", "create", "main", "-r", "@"]);
        assert!(
            bookmark.status.success(),
            "jj bookmark create failed: {bookmark:?}"
        );
        if described {
            let desc = jj_in(&dir, &["describe", "-m", "test change"]);
            assert!(desc.status.success(), "jj describe failed: {desc:?}");
        }
        dir
    }

    #[test]
    fn resolve_uses_the_smart_shift_only_when_undescribed() {
        // `resolve` shells out to the *ambient* repo, so the check has to run
        // from inside the repo. CI checks out with git and has no `.jj`, so
        // relying on the working copy being a jj repo is not an option.
        let dir = scratch_repo(true);
        // Safety: `set_current_dir` is process-global; this test is the only
        // one that mutates it and no other test shells out to jj.
        std::env::set_current_dir(&dir).expect("enter scratch repo");

        let target = resolve(Some("main")).expect("main exists here");
        assert_eq!(target.revset, "main");
        assert!(!target.base.commit_id.is_empty());
        assert!(!target.head.commit_id.is_empty());
    }

    #[test]
    fn missing_jj_is_reported() {
        // A missing `jj` must surface as a clear error, not a panic.
        let r = Command::new("jj").arg("--version").output();
        assert!(r.is_ok(), "jj should be installed for the test suite");
    }
}
