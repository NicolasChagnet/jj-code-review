//! Parser for git-style unified diffs, as emitted by `jj diff --git`.
//!
//! A file entry starts at a `diff --git a/<old> b/<new>` header and runs until
//! the next such header. Paths come from that header plus `rename from/to` and
//! `copy from/to` lines; `---`/`+++` are only inspected for `/dev/null`.
//! Hunk bodies record one [`DiffLine`] per content line with explicit old/new
//! numbers; the `\ No newline at end of file` marker is folded into the
//! preceding line's `no_newline` flag rather than stored as a line of its own.
//! Malformed and unrecognised lines are skipped, never fatal.

/// How a file changed between the two revisions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    /// A new file. Also the fallback when no metadata line identifies the change.
    #[default]
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}

/// One changed file, with its optional hunks in file order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDiff {
    /// New-side path; for deletions, the old-side path (they are identical).
    pub path: String,
    /// Pre-image path, present only for renames and copies.
    pub old_path: Option<String>,
    pub status: Status,
    /// True when the payload was elided (`Binary files ... differ`).
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

/// Role of a single body line within a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineKind {
    /// Unchanged line, present in both revisions.
    #[default]
    Context,
    Add,
    Del,
}

/// One body line, with its position on each side it exists on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Pre-image line number; `None` for additions.
    pub old_ln: Option<u32>,
    /// Post-image line number; `None` for deletions.
    pub new_ln: Option<u32>,
    /// Content with the leading marker character stripped, byte-exact.
    pub text: String,
    /// True when the file ends without a trailing newline right after this line.
    pub no_newline: bool,
}

/// A contiguous run of changes, described by its `@@` header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// The raw `@@ -a,b +c,d @@ optional heading` line.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// Index of the first line in this hunk that exists on the new side.
    #[cfg(test)]
    pub fn first_new_line(&self) -> Option<usize> {
        self.lines.iter().position(|l| l.new_ln.is_some())
    }

    /// Index of the first line in this hunk that exists on the old side.
    #[cfg(test)]
    pub fn first_old_line(&self) -> Option<usize> {
        self.lines.iter().position(|l| l.old_ln.is_some())
    }
}

/// Parse git-style unified diff text into per-file diffs.
///
/// Text before the first `diff --git` header is ignored; malformed lines are
/// skipped rather than reported, so this never fails.
pub fn parse(raw: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    // Open hunk plus the running line counters for its body.
    let mut in_hunk: Option<(Hunk, u32, u32)> = None;

    for line in raw.split('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            finish(&mut current, &mut in_hunk, &mut files);
            current = Some(new_file(rest));
            continue;
        }

        // A `@@`-prefixed line opens a new hunk unless the open hunk still has
        // body lines left to consume. This keeps a context line whose text
        // begins with `@@` (and so starts with `@@ `) from ending the hunk.
        if line.starts_with("@@ ") && hunk_complete(in_hunk.as_ref()) {
            let Some(file) = current.as_mut() else {
                continue;
            };
            if let Some((hunk, _, _)) = in_hunk.take() {
                file.hunks.push(hunk);
            }
            if let Some(hunk) = parse_hunk_header(line) {
                let cursors = (hunk.old_start, hunk.new_start);
                in_hunk = Some((hunk, cursors.0, cursors.1));
            }
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };

        let Some((hunk, old_cursor, new_cursor)) = in_hunk.as_mut() else {
            parse_metadata(line, file);
            continue;
        };

        // No-newline marker: annotates the line it follows, emits nothing.
        if line == "\\ No newline at end of file" {
            if let Some(last) = hunk.lines.last_mut() {
                last.no_newline = true;
            }
            continue;
        }

        let (marker, text) = match line.split_at_checked(1) {
            Some((marker, text)) => (marker, text),
            // Empty line: not a body line.
            None => continue,
        };

        let line_model = match marker {
            "+" => Some(DiffLine {
                kind: LineKind::Add,
                old_ln: None,
                new_ln: Some(0),
                text: text.to_string(),
                no_newline: false,
            }),
            // A `@@ ` line that did not open a hunk is body content whose text
            // happens to start with `@@`, i.e. a context line whose marker is
            // the space before `@@`.
            "@" if text.starts_with("@ ") => Some(DiffLine {
                kind: LineKind::Context,
                old_ln: Some(0),
                new_ln: Some(0),
                text: format!("@{text}"),
                no_newline: false,
            }),
            "-" => Some(DiffLine {
                kind: LineKind::Del,
                old_ln: Some(0),
                new_ln: None,
                text: text.to_string(),
                no_newline: false,
            }),
            " " => Some(DiffLine {
                kind: LineKind::Context,
                old_ln: Some(0),
                new_ln: Some(0),
                text: text.to_string(),
                no_newline: false,
            }),
            // `\ No newline ...` is handled above; anything else (notably the
            // `diff --git` guard and trailing garbage) is skipped.
            _ => None,
        };

        if let Some(mut parsed) = line_model {
            if parsed.old_ln.is_some() {
                parsed.old_ln = Some(*old_cursor);
                *old_cursor += 1;
            }
            if parsed.new_ln.is_some() {
                parsed.new_ln = Some(*new_cursor);
                *new_cursor += 1;
            }
            hunk.lines.push(parsed);
        }
    }

    finish(&mut current, &mut in_hunk, &mut files);
    files
}

/// True when the open hunk has consumed every line its header declared, so the
/// next `@@` line must be a new hunk rather than a body line.
fn hunk_complete(in_hunk: Option<&(Hunk, u32, u32)>) -> bool {
    match in_hunk {
        None => true,
        Some((hunk, old_cursor, new_cursor)) => {
            *old_cursor >= hunk.old_start.saturating_add(hunk.old_len)
                && *new_cursor >= hunk.new_start.saturating_add(hunk.new_len)
        }
    }
}

/// Close the entry in progress, attaching any pending hunk.
fn finish(
    current: &mut Option<FileDiff>,
    in_hunk: &mut Option<(Hunk, u32, u32)>,
    files: &mut Vec<FileDiff>,
) {
    let Some(mut file) = current.take() else {
        return;
    };
    if let Some((hunk, _, _)) = in_hunk.take() {
        file.hunks.push(hunk);
    }
    // Binary payloads carry no hunks; guard against stray hunk headers.
    if file.binary {
        file.hunks.clear();
    }
    files.push(file);
}

/// Seed a file entry from a `diff --git a/<old> b/<new>` header.
fn new_file(rest: &str) -> FileDiff {
    let (old, new) = split_git_header(rest.trim_end_matches('\r'));
    FileDiff {
        path: new.unwrap_or_else(|| old.clone().unwrap_or_default()),
        old_path: None,
        status: Status::Modified,
        binary: false,
        hunks: Vec::new(),
    }
}

/// Split `a/<old> b/<new>`, tolerating unquoted paths with spaces.
fn split_git_header(rest: &str) -> (Option<String>, Option<String>) {
    for (i, _) in rest.match_indices(" b/") {
        let old = &rest[..i];
        let new = &rest[i + 1..];
        if let (Some(old), Some(new)) = (old.strip_prefix("a/"), new.strip_prefix("b/")) {
            return (Some(old.to_string()), Some(new.to_string()));
        }
    }
    (None, None)
}

/// Apply one metadata line occurring before the first hunk.
fn parse_metadata(line: &str, file: &mut FileDiff) {
    if line.starts_with("new file mode ") {
        file.set_status(Status::Added, None);
    } else if line.starts_with("deleted file mode ") {
        file.set_status(Status::Deleted, None);
    } else if let Some(path) = line.strip_prefix("rename from ") {
        file.set_status(Status::Renamed, Some(path.to_string()));
    } else if let Some(path) = line.strip_prefix("rename to ") {
        file.path = path.to_string();
    } else if let Some(path) = line.strip_prefix("copy from ") {
        file.set_status(Status::Copied, Some(path.to_string()));
    } else if let Some(path) = line.strip_prefix("copy to ") {
        file.path = path.to_string();
    } else if line == "--- /dev/null" {
        if file.status == Status::Modified {
            file.status = Status::Added;
        }
    } else if line == "+++ /dev/null" {
        if file.status == Status::Modified {
            file.status = Status::Deleted;
        }
    } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
        file.binary = true;
    }
    // Everything else (`similarity index`, `index`, mode lines, `---`/`+++`
    // paths) is intentionally ignored.
}

impl FileDiff {
    /// True when the file no longer exists on the new side.
    #[cfg(test)]
    pub fn is_deleted(&self) -> bool {
        self.status == Status::Deleted
    }

    /// Index of the first hunk holding any content, if there is one.
    #[cfg(test)]
    pub fn first_changed_hunk(&self) -> Option<usize> {
        self.hunks.iter().position(|h| !h.lines.is_empty())
    }

    /// Set the status, keeping the pre-image path only for rename/copy.
    fn set_status(&mut self, status: Status, old_path: Option<String>) {
        self.status = status;
        if old_path.is_some() {
            self.old_path = old_path;
        }
    }
}

/// Parse `@@ -o[,n] +m[,k] @@[ heading]` into an empty hunk; the body parse
/// advances separate cursors so the header's start/length stay verbatim.
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _heading) = rest.split_once(" @@")?;
    let (old_range, new_range) = ranges.split_once(" +")?;
    let (old_start, old_len) = parse_range(old_range)?;
    let (new_start, new_len) = parse_range(new_range)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        header: line.to_string(),
        lines: Vec::new(),
    })
}

/// Parse `start[,len]` with the unified-diff default of 1 when `len` is absent.
fn parse_range(range: &str) -> Option<(u32, u32)> {
    let (start, len) = match range.split_once(',') {
        Some((start, len)) => (start, len.parse().ok()?),
        None => (range, 1),
    };
    Some((start.parse().ok()?, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODIFY: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@ fn main
 fn main() {
-    old();
+    new();
+    more();
 }
";

    const ADD: &str = "\
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,2 @@
+alpha
+beta
";

    const DELETE: &str = "\
diff --git a/src/gone.rs b/src/gone.rs
deleted file mode 100644
index 3333333..0000000
--- a/src/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-alpha
-beta
";

    const RENAME: &str = "\
diff --git a/src/old.rs b/src/new_name.rs
similarity index 80%
rename from src/old.rs
rename to src/new_name.rs
index 1111111..2222222 100644
--- a/src/old.rs
+++ b/src/new_name.rs
@@ -1,2 +1,2 @@
 keep
-was
+is
";

    const COPY: &str = "\
diff --git a/src/a.rs b/src/b.rs
similarity index 100%
copy from src/a.rs
copy to src/b.rs
";

    const BINARY: &str = "\
diff --git a/logo.png b/logo.png
index 1111111..2222222 100644
Binary files a/logo.png and b/logo.png differ
";

    #[test]
    fn modify_numbers_every_line() {
        let files = parse(MODIFY);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.old_path, None);
        assert_eq!(file.status, Status::Modified);
        assert!(!file.binary);
        assert_eq!(file.hunks.len(), 1);

        let hunk = &file.hunks[0];
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_len,
                hunk.new_start,
                hunk.new_len,
                hunk.header.as_str()
            ),
            (1, 3, 1, 4, "@@ -1,3 +1,4 @@ fn main")
        );

        let expected = [
            (LineKind::Context, Some(1), Some(1), "fn main() {"),
            (LineKind::Del, Some(2), None, "    old();"),
            (LineKind::Add, None, Some(2), "    new();"),
            (LineKind::Add, None, Some(3), "    more();"),
            (LineKind::Context, Some(3), Some(4), "}"),
        ];
        let actual: Vec<_> = hunk
            .lines
            .iter()
            .map(|l| (l.kind, l.old_ln, l.new_ln, l.text.as_str()))
            .collect();
        assert_eq!(actual, expected);
        assert!(hunk.lines.iter().all(|l| !l.no_newline));
        assert_eq!(hunk.first_new_line(), Some(0));
        assert_eq!(hunk.first_old_line(), Some(0));
        assert_eq!(file.first_changed_hunk(), Some(0));
    }

    #[test]
    fn add_has_no_old_numbers() {
        let files = parse(ADD);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.status, Status::Added);
        assert_eq!(file.path, "src/new.rs");
        assert_eq!(file.hunks.len(), 1);

        let hunk = &file.hunks[0];
        assert_eq!((hunk.old_start, hunk.old_len), (0, 0));
        assert_eq!((hunk.new_start, hunk.new_len), (1, 2));
        assert_eq!(hunk.old_len, 0);
        assert_eq!(
            hunk.lines,
            vec![
                DiffLine {
                    kind: LineKind::Add,
                    old_ln: None,
                    new_ln: Some(1),
                    text: "alpha".into(),
                    no_newline: false,
                },
                DiffLine {
                    kind: LineKind::Add,
                    old_ln: None,
                    new_ln: Some(2),
                    text: "beta".into(),
                    no_newline: false,
                },
            ]
        );
    }

    #[test]
    fn delete_has_no_new_numbers() {
        let files = parse(DELETE);
        let file = &files[0];
        assert!(file.is_deleted());
        assert_eq!(file.path, "src/gone.rs");
        assert_eq!(file.status, Status::Deleted);

        let hunk = &file.hunks[0];
        assert_eq!((hunk.old_start, hunk.old_len), (1, 2));
        assert_eq!((hunk.new_start, hunk.new_len), (0, 0));
        assert_eq!(
            hunk.lines,
            vec![
                DiffLine {
                    kind: LineKind::Del,
                    old_ln: Some(1),
                    new_ln: None,
                    text: "alpha".into(),
                    no_newline: false,
                },
                DiffLine {
                    kind: LineKind::Del,
                    old_ln: Some(2),
                    new_ln: None,
                    text: "beta".into(),
                    no_newline: false,
                },
            ]
        );
        assert!(hunk.lines.iter().all(|l| l.new_ln.is_none()));
        assert_eq!(file.first_changed_hunk(), Some(0), "hunks still hold lines");
    }

    #[test]
    fn rename_records_old_path() {
        let files = parse(RENAME);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.status, Status::Renamed);
        assert_eq!(file.path, "src/new_name.rs");
        assert_eq!(file.old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines.len(), 3);
        assert_eq!(file.hunks[0].lines[2].text, "is");
    }

    #[test]
    fn copy_records_old_path() {
        let files = parse(COPY);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.status, Status::Copied);
        assert_eq!(file.path, "src/b.rs");
        assert_eq!(file.old_path.as_deref(), Some("src/a.rs"));
        assert!(file.hunks.is_empty());
        assert!(!file.binary);
    }

    #[test]
    fn binary_has_no_hunks() {
        let files = parse(BINARY);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert!(file.binary);
        assert!(file.hunks.is_empty());
        assert_eq!(file.path, "logo.png");
    }

    #[test]
    fn added_empty_file_parses() {
        let with_hunk = "\
diff --git a/empty b/empty
new file mode 100644
index 0000000..e69de29
@@ -0,0 +0,0 @@
";
        let files = parse(with_hunk);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, Status::Added);
        assert_eq!(files[0].hunks.len(), 1);
        assert!(files[0].hunks[0].lines.is_empty());
        assert_eq!(
            (files[0].hunks[0].old_len, files[0].hunks[0].new_len),
            (0, 0)
        );

        let without_hunk = "\
diff --git a/empty b/empty
new file mode 100644
";
        let files = parse(without_hunk);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, Status::Added);
        assert!(files[0].hunks.is_empty());
        assert_eq!(files[0].path, "empty");
    }

    #[test]
    fn no_newline_marker_annotates_previous_line() {
        let raw = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let files = parse(raw);
        let hunk = &files[0].hunks[0];
        assert_eq!(hunk.lines.len(), 2);
        assert_eq!(hunk.lines[0].text, "old");
        assert_eq!(hunk.lines[1].text, "new");
        assert!(hunk.lines[0].no_newline);
        assert!(hunk.lines[1].no_newline);
        assert_eq!((hunk.old_len, hunk.new_len), (1, 1));
    }

    #[test]
    fn missing_counts_default_to_one() {
        let raw = "\
diff --git a/f b/f
@@ -5 +7 @@
 solo
";
        let files = parse(raw);
        let hunk = &files[0].hunks[0];
        assert_eq!((hunk.old_start, hunk.old_len), (5, 1));
        assert_eq!((hunk.new_start, hunk.new_len), (7, 1));
        assert_eq!(hunk.lines[0].old_ln, Some(5));
        assert_eq!(hunk.lines[0].new_ln, Some(7));
    }

    #[test]
    fn multi_file_and_at_at_context_line() {
        let raw = "\
diff --git a/one.rs b/one.rs
@@ -1,3 +1,3 @@
@@ nested marker in content
-@@ keep me honest
+@@ keep me honest too
diff --git a/two.rs b/two.rs
@@ -1 +1 @@
 x
";
        let files = parse(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "one.rs");
        assert_eq!(files[1].path, "two.rs");
        // The `@@`-prefixed context line is body content, not a hunk header.
        assert_eq!(files[0].hunks.len(), 1);
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].kind, LineKind::Context);
        assert_eq!(lines[0].text, "@@ nested marker in content");
        assert_eq!(lines[1].text, "@@ keep me honest");
        assert_eq!(lines[2].text, "@@ keep me honest too");
        assert_eq!(files[1].hunks[0].header, "@@ -1 +1 @@");
    }

    #[test]
    fn multi_hunk_numbering_restarts() {
        let raw = "\
diff --git a/f.rs b/f.rs
@@ -1,2 +1,2 @@
 a
-b
+c
@@ -10,2 +10,3 @@
 d
+e
 f
";
        let files = parse(raw);
        let hunks = &files[0].hunks;
        assert_eq!(hunks.len(), 2);

        let first: Vec<_> = hunks[0]
            .lines
            .iter()
            .map(|l| (l.old_ln, l.new_ln))
            .collect();
        assert_eq!(
            first,
            vec![(Some(1), Some(1)), (Some(2), None), (None, Some(2))]
        );

        let second: Vec<_> = hunks[1]
            .lines
            .iter()
            .map(|l| (l.old_ln, l.new_ln))
            .collect();
        assert_eq!(
            second,
            vec![(Some(10), Some(10)), (None, Some(11)), (Some(11), Some(12)),]
        );
    }

    #[test]
    fn garbage_before_first_header_is_ignored() {
        let raw = "warning: something\n\
diff --git a/f b/f
@@ -1 +1 @@
 x
trailing garbage
";
        let files = parse(raw);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 1);
    }

    #[test]
    fn empty_input_yields_no_files() {
        assert!(parse("").is_empty());
    }
}
