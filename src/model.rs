//! Review annotations and the rules that anchor them to diff rows.
//!
//! An annotation is created from a contiguous range of diff rows (rows are
//! indexed into a single [`FileDiff`](crate::diff::FileDiff), hunk separators
//! excluded by the caller). The range yields an old-side and a new-side line
//! span; both are kept for rendering, but only one side is reported. The
//! reported side is `Old` when the **last** row of the range is a deletion —
//! that range exists only in the pre-change file — and `New` otherwise. This
//! matches the anchoring rule of the reference maki plugin.
//!
//! Edits and deletions only make sense on lines that survive into the result,
//! so they are rejected on old-side ranges.

use crate::diff::{DiffLine, LineKind};

/// Which side of the diff an annotation's line numbers refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Old => "old",
            Side::New => "new",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Comment,
    Edit,
    Delete,
}

impl Kind {
    /// Sort key for the submit output: comment, then edit, then delete.
    pub fn rank(self) -> u8 {
        match self {
            Kind::Comment => 0,
            Kind::Edit => 1,
            Kind::Delete => 2,
        }
    }

    pub fn marker(self) -> &'static str {
        match self {
            Kind::Comment => "●",
            Kind::Edit => "✎",
            Kind::Delete => "✗",
        }
    }
}

/// Payload of an annotation: free text, replacement content, or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Comment(String),
    Edit(String),
    Delete,
}

impl Body {
    pub fn kind(&self) -> Kind {
        match self {
            Body::Comment(_) => Kind::Comment,
            Body::Edit(_) => Kind::Edit,
            Body::Delete => Kind::Delete,
        }
    }
}

/// One annotation, anchored to a file and a line span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// Index into the diff's file list.
    pub file: usize,
    /// Index of the first anchored diff row (a row inside a hunk).
    pub anchor_row: usize,
    /// Number of diff rows from `anchor_row` covered by the range.
    pub row_count: usize,
    /// Line span on the reported [`Side`], 1-based and inclusive.
    pub side: Side,
    pub start: u32,
    pub end: u32,
    /// Old-side span, when the range covers old lines.
    pub old_span: Option<(u32, u32)>,
    /// New-side span, when the range covers new lines.
    pub new_span: Option<(u32, u32)>,
    pub body: Body,
}

impl Annotation {
    pub fn kind(&self) -> Kind {
        self.body.kind()
    }

    /// Content-row index one past the last row of the anchored range.
    pub fn max_row(&self) -> usize {
        self.anchor_row + self.row_count.saturating_sub(1)
    }

    /// True when this annotation covers the given content row and line.
    pub fn covers(&self, row: usize, line: &DiffLine) -> bool {
        if row < self.anchor_row || row > self.max_row() {
            return false;
        }
        match line.kind {
            LineKind::Del => match self.old_span {
                Some((s, e)) => line.old_ln.is_some_and(|n| n >= s && n <= e),
                None => false,
            },
            _ => match self.new_span {
                Some((s, e)) => line.new_ln.is_some_and(|n| n >= s && n <= e),
                None => false,
            },
        }
    }

    /// Human label like `L12-14` or `removed L7`.
    pub fn label(&self) -> String {
        let prefix = match self.side {
            Side::New => String::from("L"),
            Side::Old => String::from("removed L"),
        };
        if self.start == self.end {
            format!("{prefix}{}", self.start)
        } else {
            format!("{prefix}{}-{}", self.start, self.end)
        }
    }
}

/// Why an annotation could not be created from a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorError {
    /// The range contains no diff lines at all.
    Empty,
    /// The range is anchored to the old side, which no longer exists.
    OldSide,
}

/// Builds an annotation from `rows[from..=to]`, or explains why it cannot.
pub fn anchor(
    file: usize,
    rows: &[DiffLine],
    from: usize,
    to: usize,
    body: Body,
) -> Result<Annotation, AnchorError> {
    if from > to || to >= rows.len() {
        return Err(AnchorError::Empty);
    }

    let mut old: Option<(u32, u32)> = None;
    let mut new: Option<(u32, u32)> = None;
    for line in &rows[from..=to] {
        if let Some(n) = line.old_ln {
            old = Some(span(old, n));
        }
        if let Some(n) = line.new_ln {
            new = Some(span(new, n));
        }
    }

    if old.is_none() && new.is_none() {
        return Err(AnchorError::Empty);
    }

    // A range ending on a deletion has no counterpart in the result.
    let side = if rows[to].kind == LineKind::Del {
        Side::Old
    } else {
        Side::New
    };
    let (start, end) = match side {
        Side::Old => old.ok_or(AnchorError::Empty)?,
        Side::New => new.ok_or(AnchorError::Empty)?,
    };

    if side == Side::Old && matches!(body, Body::Edit(_) | Body::Delete) {
        return Err(AnchorError::OldSide);
    }

    Ok(Annotation {
        file,
        anchor_row: from,
        row_count: to - from + 1,
        side,
        start,
        end,
        old_span: old,
        new_span: new,
        body,
    })
}

fn span(current: Option<(u32, u32)>, n: u32) -> (u32, u32) {
    match current {
        None => (n, n),
        Some((s, e)) => (s.min(n), e.max(n)),
    }
}

/// New-side content of an annotation's range, for prefilling an edit popup.
pub fn new_side_content(rows: &[DiffLine], from: usize, to: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in &rows[from.min(rows.len())..=to.min(rows.len().saturating_sub(1))] {
        if line.new_ln.is_some() {
            out.push(&line.text);
        }
    }
    out.join("\n")
}

/// Sorts annotations into the deterministic submit order: file order, then
/// start line, then kind (comment, edit, delete).
pub fn submit_order(annotations: &mut [Annotation]) {
    annotations.sort_by_key(|a| (a.file, a.start, a.end, a.kind().rank()));
}

/// All files that carry at least one annotation, in diff order.
pub fn annotated_files(annotations: &[Annotation]) -> Vec<usize> {
    let mut files: Vec<usize> = annotations.iter().map(|a| a.file).collect();
    files.sort_unstable();
    files.dedup();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;

    fn rows(raw: &str) -> Vec<DiffLine> {
        let files = diff::parse(raw);
        let mut out = Vec::new();
        for h in &files[0].hunks {
            out.extend(h.lines.iter().cloned());
        }
        out
    }

    const MODIFY: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,6 +10,7 @@ fn main() {
 context ten
 context eleven
-old twelve
-old thirteen
+new twelve
+new thirteen
+new fourteen
 context fifteen
 context sixteen
";

    #[test]
    fn single_new_line_comment() {
        let r = rows(MODIFY);
        // rows: 0..=6, "+new fourteen" is row 6 (new line 14)
        let a = anchor(0, &r, 6, 6, Body::Comment("hi".into())).unwrap();
        assert_eq!(a.side, Side::New);
        assert_eq!((a.start, a.end), (14, 14));
        assert_eq!(a.label(), "L14");
        assert_eq!(a.new_span, Some((14, 14)));
        assert_eq!(a.old_span, None);
    }

    #[test]
    fn range_of_new_lines_uses_min_and_max() {
        let r = rows(MODIFY);
        let a = anchor(0, &r, 4, 6, Body::Comment("hi".into())).unwrap();
        assert_eq!(a.side, Side::New);
        assert_eq!((a.start, a.end), (12, 14));
        assert_eq!(a.label(), "L12-14");
    }

    #[test]
    fn range_ending_on_deletion_anchors_old() {
        let r = rows(MODIFY);
        let a = anchor(0, &r, 2, 3, Body::Comment("gone?".into())).unwrap();
        assert_eq!(a.side, Side::Old);
        assert_eq!((a.start, a.end), (12, 13), "old line numbers");
        assert_eq!(a.label(), "removed L12-13");
        assert_eq!(a.old_span, Some((12, 13)));
    }

    #[test]
    fn mixed_range_ending_on_new_anchors_new() {
        let r = rows(MODIFY);
        // rows 3 (old 13) through 4 (new 12)
        let a = anchor(0, &r, 3, 4, Body::Comment("hi".into())).unwrap();
        assert_eq!(a.side, Side::New);
        assert_eq!((a.start, a.end), (12, 12));
        assert_eq!(a.old_span, Some((13, 13)));
        assert_eq!(a.new_span, Some((12, 12)));
    }

    #[test]
    fn context_range_anchors_new() {
        let r = rows(MODIFY);
        let a = anchor(0, &r, 0, 1, Body::Comment("hi".into())).unwrap();
        assert_eq!(a.side, Side::New);
        assert_eq!((a.start, a.end), (10, 11));
    }

    #[test]
    fn edit_rejected_on_old_side() {
        let r = rows(MODIFY);
        assert_eq!(
            anchor(0, &r, 2, 3, Body::Edit("x".into())),
            Err(AnchorError::OldSide)
        );
        assert_eq!(anchor(0, &r, 2, 3, Body::Delete), Err(AnchorError::OldSide));
    }

    #[test]
    fn edit_allowed_on_new_side() {
        let r = rows(MODIFY);
        let a = anchor(0, &r, 4, 5, Body::Edit("a\nb".into())).unwrap();
        assert_eq!(a.kind(), Kind::Edit);
        assert_eq!((a.start, a.end), (12, 13));
    }

    #[test]
    fn empty_and_out_of_range_ranges_fail() {
        let r = rows(MODIFY);
        assert_eq!(anchor(0, &r, 2, 1, Body::Delete), Err(AnchorError::Empty));
        assert_eq!(anchor(0, &r, 0, 99, Body::Delete), Err(AnchorError::Empty));
        assert!(anchor(0, &[], 0, 0, Body::Delete).is_err());
    }

    #[test]
    fn added_file_all_new_lines() {
        let raw = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
";
        let r = rows(raw);
        let a = anchor(0, &r, 0, 1, Body::Comment("hi".into())).unwrap();
        assert_eq!(a.side, Side::New);
        assert_eq!((a.start, a.end), (1, 2));
    }

    #[test]
    fn no_newline_marker_is_folded_into_the_line() {
        let raw = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let r = rows(raw);
        assert_eq!(r.len(), 2);
        assert!(r[0].no_newline && r[1].no_newline);
        let a = anchor(0, &r, 1, 1, Body::Delete).unwrap();
        assert_eq!(a.side, Side::New);
        assert_eq!((a.start, a.end), (1, 1));
    }

    #[test]
    fn new_side_content_collects_replacement_lines() {
        let r = rows(MODIFY);
        assert_eq!(
            new_side_content(&r, 4, 6),
            "new twelve\nnew thirteen\nnew fourteen"
        );
        assert_eq!(new_side_content(&r, 0, 1), "context ten\ncontext eleven");
    }

    #[test]
    fn submit_order_is_file_then_start_then_kind() {
        let mut a = vec![
            Annotation {
                file: 1,
                anchor_row: 0,
                row_count: 1,
                side: Side::New,
                start: 2,
                end: 2,
                old_span: None,
                new_span: Some((2, 2)),
                body: Body::Delete,
            },
            Annotation {
                file: 0,
                anchor_row: 0,
                row_count: 1,
                side: Side::New,
                start: 9,
                end: 9,
                old_span: None,
                new_span: Some((9, 9)),
                body: Body::Comment("b".into()),
            },
            Annotation {
                file: 0,
                anchor_row: 0,
                row_count: 1,
                side: Side::New,
                start: 9,
                end: 9,
                old_span: None,
                new_span: Some((9, 9)),
                body: Body::Edit("x".into()),
            },
        ];
        submit_order(&mut a);
        assert_eq!(a[0].file, 0);
        assert_eq!(a[0].kind(), Kind::Comment);
        assert_eq!(a[1].kind(), Kind::Edit);
        assert_eq!(a[2].file, 1);
    }

    #[test]
    fn annotated_files_lists_each_file_once() {
        let rows = rows(MODIFY);
        let annotations = vec![
            anchor(0, &rows, 6, 6, Body::Comment("a".into())).unwrap(),
            anchor(0, &rows, 5, 5, Body::Edit("b".into())).unwrap(),
            anchor(0, &rows, 5, 5, Body::Delete).unwrap(),
        ];
        assert_eq!(annotated_files(&annotations), vec![0]);
    }

    #[test]
    fn covers_matches_rows_by_side() {
        let r = rows(MODIFY);
        let a = anchor(0, &r, 4, 6, Body::Comment("hi".into())).unwrap();
        assert!(a.covers(4, &r[4]));
        assert!(a.covers(6, &r[6]));
        assert!(!a.covers(3, &r[3]), "old-13 row is outside the new span");

        let old = anchor(0, &r, 2, 3, Body::Comment("hi".into())).unwrap();
        assert!(old.covers(2, &r[2]));
        assert!(old.covers(3, &r[3]));
        assert!(!old.covers(4, &r[4]));
    }
}
