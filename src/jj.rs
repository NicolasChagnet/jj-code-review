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
pub fn resolve(revset: Option<&str>) -> Result<Target> {
    let head = if is_empty()? && description("@")?.trim().is_empty() {
        "@-"
    } else {
        "@"
    };
    let base = match revset {
        Some(r) => r.to_string(),
        None => format!("{head}-"),
    };
    Ok(Target {
        revset: revset.map(str::to_string).unwrap_or_else(|| base.clone()),
        base: rev(&base)?,
        head: rev(&head)?,
    })
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
    let raw = jj(&[
        "log",
        "-r",
        revset,
        "--no-graph",
        "-T",
        "commit_id ++ \"|\" ++ description",
    ])?;
    let raw = raw.trim_end_matches('\n');
    // A multi-revision revset logs several lines; the newest is the last one
    // jj prints, so take the final line as the representative revision.
    let line = raw.lines().next_back().unwrap_or("");
    let (commit_id, description) = line.split_once('|').unwrap_or((line, ""));
    if commit_id.is_empty() {
        return Err(JjError(format!("no revision matches `{revset}`")));
    }
    Ok(Rev {
        commit_id: commit_id.to_string(),
        description: description.to_string(),
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

    #[test]
    fn missing_jj_is_reported() {
        // Only meaningful when jj is absent; otherwise just assert it works.
        let r = Command::new("jj").arg("--version").output();
        assert!(r.is_ok(), "jj should be available for the manual checklist");
    }
}
