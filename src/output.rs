//! Rendering of a finished review session.
//!
//! Two flavours of the same data: a human-readable plain-text report and a
//! machine-readable JSON document. Both start by putting the annotations into
//! [`model::submit_order`], so the output is deterministic (file order, then
//! start line, then kind). `file` is an index into the diff's file list; an
//! index outside it is skipped rather than panicking.

use serde::Serialize;

use crate::{diff, jj, model};

pub fn render_text(
    target: &jj::Target,
    files: &[diff::FileDiff],
    annotations: &mut [model::Annotation],
) -> String {
    model::submit_order(annotations);

    let header = format!(
        "Review of {} \"{}\" ({}..{})",
        target.head.short(),
        target.head.short_description(),
        target.base.short(),
        target.head.short()
    );

    let sections: Vec<String> = model::annotated_files(annotations)
        .into_iter()
        .filter_map(|file| files.get(file).map(|diff| (file, diff)))
        .map(|(index, file)| {
            let entries = annotations.iter().filter(|a| a.file == index);
            section(file, entries)
        })
        .collect();

    if sections.is_empty() {
        return format!("{header}: no findings.\n");
    }
    format!("{header}\n\n{}\n", sections.join("\n\n"))
}

/// Header line plus the annotation entries of one file.
fn section<'a, I>(file: &diff::FileDiff, annotations: I) -> String
where
    I: Iterator<Item = &'a model::Annotation>,
{
    let mut out = header_line(file);
    for annotation in annotations {
        out.push('\n');
        out.push_str(&entry(annotation));
    }
    out
}

fn header_line(file: &diff::FileDiff) -> String {
    let origin = match (&file.old_path, &file.status) {
        (Some(old), diff::Status::Copied) => Some(("copied from", old)),
        (Some(old), diff::Status::Renamed) => Some(("renamed from", old)),
        _ => None,
    };
    match origin {
        Some((verb, old)) => format!("{} ({verb} {old})", file.path),
        None => file.path.clone(),
    }
}

/// One annotation, indented two spaces. A whole-file comment reads
/// `whole file:` rather than carrying line numbers it does not have.
///
/// The quoted diff follows the entry, indented to match, so the report is
/// readable without the diff in front of the reader.
fn entry(annotation: &model::Annotation) -> String {
    let label = annotation.label();
    let mut out = match &annotation.body {
        model::Body::Comment(text) if annotation.is_file_scope() => {
            let lines = trimmed_lines(text);
            let mut out = format!("  whole file: {}", lines.first().copied().unwrap_or(""));
            for line in &lines[1..] {
                out.push_str(&format!("\n    | {line}"));
            }
            out
        }
        model::Body::Comment(text) => {
            let lines = trimmed_lines(text);
            let mut out = format!("  {label}: {}", lines.first().copied().unwrap_or(""));
            for line in &lines[1..] {
                out.push_str(&format!("\n    | {line}"));
            }
            out
        }
        model::Body::Edit(text) => {
            let mut out = format!("  {label} edit:");
            for line in trimmed_lines(text) {
                out.push_str(&format!("\n    | {line}"));
            }
            out
        }
        model::Body::Delete => format!("  {label} delete"),
    };
    for line in annotation.snippet.lines() {
        out.push_str(&format!("\n      {line}"));
    }
    out
}

/// Content lines with trailing blanks dropped, so a text ending in a newline
/// does not render a spurious empty row.
fn trimmed_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

/// Serialisable mirror of the report. `old_path` is a plain `Option`, which
/// serialises to `null` when absent, as the schema requires.
#[derive(Serialize)]
struct JsonReport<'a> {
    revset: &'a str,
    base: &'a str,
    head: &'a str,
    head_description: &'a str,
    files: Vec<JsonFile<'a>>,
}

#[derive(Serialize)]
struct JsonFile<'a> {
    path: &'a str,
    old_path: Option<&'a str>,
    /// Comments on the file as a whole; these carry no line numbers.
    file_comments: Vec<JsonFileComment<'a>>,
    comments: Vec<JsonComment<'a>>,
    edits: Vec<JsonEdit<'a>>,
    deletions: Vec<JsonDeletion<'a>>,
}

#[derive(Serialize)]
struct JsonFileComment<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct JsonComment<'a> {
    /// `null` never appears here: whole-file comments use `file_comments`.
    side: Option<&'a str>,
    start: u32,
    end: u32,
    text: &'a str,
    /// The diff text the annotation points at, with the reviewed lines marked.
    snippet: &'a str,
}

#[derive(Serialize)]
struct JsonEdit<'a> {
    side: Option<&'a str>,
    start: u32,
    end: u32,
    content: &'a str,
    snippet: &'a str,
}

#[derive(Serialize)]
struct JsonDeletion<'a> {
    side: Option<&'static str>,
    start: u32,
    end: u32,
    snippet: &'a str,
}

pub fn render_json(
    target: &jj::Target,
    files: &[diff::FileDiff],
    annotations: &mut [model::Annotation],
) -> String {
    model::submit_order(annotations);

    let files = model::annotated_files(annotations)
        .into_iter()
        .filter_map(|index| files.get(index).map(|file| (index, file)))
        .map(|(index, file)| JsonFile {
            path: &file.path,
            old_path: file.old_path.as_deref(),
            file_comments: collect(annotations, index, |a, body| match body {
                model::Body::Comment(text) if a.is_file_scope() => Some(JsonFileComment { text }),
                _ => None,
            }),
            comments: collect(annotations, index, |a, body| match body {
                model::Body::Comment(text) if !a.is_file_scope() => Some(JsonComment {
                    side: a.side.as_str(),
                    start: a.start,
                    end: a.end,
                    text,
                    snippet: &a.snippet,
                }),
                _ => None,
            }),
            edits: collect(annotations, index, |a, body| match body {
                model::Body::Edit(content) => Some(JsonEdit {
                    side: a.side.as_str(),
                    start: a.start,
                    end: a.end,
                    content,
                    snippet: &a.snippet,
                }),
                _ => None,
            }),
            deletions: collect(annotations, index, |a, body| match body {
                model::Body::Delete => Some(JsonDeletion {
                    side: a.side.as_str(),
                    start: a.start,
                    end: a.end,
                    snippet: &a.snippet,
                }),
                _ => None,
            }),
        })
        .collect();

    let report = JsonReport {
        revset: &target.revset,
        base: target.base.short(),
        head: target.head.short(),
        head_description: target.head.short_description(),
        files,
    };
    // Serialisation of this struct cannot fail.
    let mut json = serde_json::to_string_pretty(&report).unwrap_or_default();
    json.push('\n');
    json
}

/// Projects the annotations of one file through `f`, keeping non-matches out.
fn collect<'a, T>(
    annotations: &'a [model::Annotation],
    file: usize,
    f: impl Fn(&'a model::Annotation, &'a model::Body) -> Option<T>,
) -> Vec<T> {
    annotations
        .iter()
        .filter(|a| a.file == file)
        .filter_map(|a| f(a, &a.body))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{FileDiff, Status};
    use crate::jj::Rev;
    use crate::model::{Annotation, Body, Side};
    use serde_json::Value;

    fn target() -> jj::Target {
        jj::Target {
            revset: "@-".into(),
            base: Rev {
                commit_id: "0000000000000000".into(),
                description: "base".into(),
            },
            head: Rev {
                commit_id: "42d29bfe9fd6abc".into(),
                description: "initial setup".into(),
            },
        }
    }

    fn file(path: &str, old_path: Option<&str>, status: Status) -> FileDiff {
        FileDiff {
            path: path.into(),
            old_path: old_path.map(str::to_string),
            status,
            binary: false,
            hunks: Vec::new(),
        }
    }

    /// Whole-file comment, as `jcr` builds them from the Files pane.
    fn file_comment(file: usize, text: &str) -> Annotation {
        crate::model::file_anchor(file, Body::Comment(text.into())).expect("comments are allowed")
    }

    fn annotation(file: usize, start: u32, end: u32, side: Side, body: Body) -> Annotation {
        let span = (start, end);
        Annotation {
            file,
            anchor_row: 0,
            row_count: (end - start) as usize + 1,
            side,
            start,
            end,
            old_span: Some(span),
            new_span: Some(span),
            snippet: format!("@@ -{start},1 +{start},1 @@\n ctx\n-reviewed\n+reviewed"),
            body,
        }
    }

    fn files() -> Vec<FileDiff> {
        vec![
            file("src/main.rs", None, Status::Modified),
            file("src/old.rs", Some("src/older.rs"), Status::Renamed),
        ]
    }

    #[test]
    fn text_report_lists_comment_edit_and_delete() {
        let mut annotations = vec![
            annotation(0, 33, 35, Side::New, Body::Delete),
            annotation(
                1,
                7,
                7,
                Side::Old,
                Body::Comment("why was this removed?".into()),
            ),
            annotation(0, 30, 31, Side::New, Body::Edit("line 1\nline 2".into())),
            annotation(
                0,
                12,
                14,
                Side::New,
                Body::Comment("This loop is O(n²); consider a HashMap.".into()),
            ),
        ];

        let expected = "\
Review of 42d29bfe9fd6 \"initial setup\" (000000000000..42d29bfe9fd6)

src/main.rs
  L12-14: This loop is O(n²); consider a HashMap.
      @@ -12,1 +12,1 @@
       ctx
      -reviewed
      +reviewed
  L30-31 edit:
    | line 1
    | line 2
      @@ -30,1 +30,1 @@
       ctx
      -reviewed
      +reviewed
  L33-35 delete
      @@ -33,1 +33,1 @@
       ctx
      -reviewed
      +reviewed

src/old.rs (renamed from src/older.rs)
  removed L7: why was this removed?
      @@ -7,1 +7,1 @@
       ctx
      -reviewed
      +reviewed
";
        assert_eq!(render_text(&target(), &files(), &mut annotations), expected);
    }

    #[test]
    fn text_report_without_annotations_says_no_findings() {
        let mut annotations = Vec::new();
        assert_eq!(
            render_text(&target(), &files(), &mut annotations),
            "Review of 42d29bfe9fd6 \"initial setup\" (000000000000..42d29bfe9fd6): no findings.\n"
        );
    }

    #[test]
    fn copied_file_header_names_the_source() {
        let files = vec![file("src/b.rs", Some("src/a.rs"), Status::Copied)];
        let mut annotations = vec![annotation(0, 1, 1, Side::New, Body::Delete)];
        let out = render_text(&target(), &files, &mut annotations);
        assert!(out.contains("src/b.rs (copied from src/a.rs)\n"), "{out}");
    }

    #[test]
    fn multi_line_comment_uses_pipe_rows_and_drops_trailing_blank() {
        let mut annotations = vec![annotation(
            0,
            10,
            10,
            Side::New,
            Body::Comment("first\nsecond\n\n".into()),
        )];
        let out = render_text(&target(), &files(), &mut annotations);
        assert!(out.contains("  L10: first\n    | second\n"), "{out}");
        assert!(!out.contains("| \n"), "no blank row: {out}");
    }

    #[test]
    fn out_of_range_file_index_is_skipped() {
        let mut annotations = vec![
            annotation(9, 1, 1, Side::New, Body::Comment("nowhere".into())),
            annotation(0, 1, 1, Side::New, Body::Comment("here".into())),
        ];
        let out = render_text(&target(), &files(), &mut annotations);
        assert!(!out.contains("nowhere"));
        assert!(out.contains("  L1: here"));

        let json: Value =
            serde_json::from_str(&render_json(&target(), &files(), &mut annotations)).unwrap();
        assert_eq!(json["files"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn order_is_deterministic_regardless_of_push_order() {
        let mut annotations: Vec<_> = vec![]
            .into_iter()
            .chain([annotation(1, 7, 7, Side::Old, Body::Delete)])
            .chain([
                annotation(0, 5, 5, Side::New, Body::Delete),
                annotation(0, 5, 5, Side::New, Body::Comment("c".into())),
                annotation(0, 5, 5, Side::New, Body::Edit("e".into())),
            ])
            .collect();
        let out = render_text(&target(), &files(), &mut annotations);
        let positions: Vec<usize> = ["  L5: c", "  L5 edit:", "  L5 delete"]
            .iter()
            .map(|needle| out.find(needle).expect("entry present"))
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{out}");
        assert!(
            out.contains("\nsrc/main.rs\n")
                && out.contains("\nsrc/old.rs (renamed from src/older.rs)\n"),
            "{out}"
        );
        assert!(out.contains("  removed L7 delete\n"), "{out}");
    }

    #[test]
    fn json_report_has_expected_shape() {
        let mut annotations = vec![
            annotation(0, 12, 14, Side::New, Body::Comment("one".into())),
            annotation(0, 30, 31, Side::New, Body::Edit("line 1\nline 2".into())),
            annotation(0, 33, 35, Side::New, Body::Delete),
            annotation(1, 7, 7, Side::Old, Body::Comment("gone".into())),
        ];
        let raw = render_json(&target(), &files(), &mut annotations);
        assert!(raw.ends_with('\n'));

        let json: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["revset"], "@-");
        assert_eq!(json["base"], "000000000000");
        assert_eq!(json["head"], "42d29bfe9fd6");
        assert_eq!(json["head_description"], "initial setup");

        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["path"], "src/main.rs");
        assert!(files[0]["old_path"].is_null());
        assert_eq!(files[0]["comments"].as_array().unwrap().len(), 1);
        assert_eq!(files[0]["edits"].as_array().unwrap().len(), 1);
        assert_eq!(files[0]["deletions"].as_array().unwrap().len(), 1);
        assert_eq!(files[0]["comments"][0]["side"], "new");
        assert_eq!(files[0]["comments"][0]["start"], 12);
        assert_eq!(files[0]["comments"][0]["end"], 14);
        assert_eq!(files[0]["comments"][0]["text"], "one");
        assert_eq!(files[0]["edits"][0]["content"], "line 1\nline 2");
        assert_eq!(files[0]["deletions"][0]["side"], "new");
        assert_eq!(files[0]["deletions"][0]["start"], 33);
        assert_eq!(files[0]["deletions"][0]["end"], 35);
        assert!(
            files[0]["deletions"][0]["snippet"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "deletions carry the quoted diff"
        );

        assert_eq!(files[1]["path"], "src/old.rs");
        assert_eq!(files[1]["old_path"], "src/older.rs");
        assert_eq!(files[1]["comments"][0]["side"], "old");
        assert!(files[1]["edits"].as_array().unwrap().is_empty());
    }

    #[test]
    fn json_omits_files_without_annotations() {
        let mut annotations = vec![annotation(1, 1, 1, Side::New, Body::Delete)];
        let json: Value =
            serde_json::from_str(&render_json(&target(), &files(), &mut annotations)).unwrap();
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "src/old.rs");
        assert_eq!(files[0]["deletions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn json_without_annotations_has_empty_files() {
        let mut annotations = Vec::new();
        let json: Value =
            serde_json::from_str(&render_json(&target(), &files(), &mut annotations)).unwrap();
        assert!(json["files"].as_array().unwrap().is_empty());
    }

    #[test]
    fn json_puts_whole_file_comments_in_their_own_array() {
        let mut annotations = vec![
            file_comment(0, "this file needs tests"),
            annotation(0, 12, 14, Side::New, Body::Comment("line comment".into())),
        ];
        let json: Value =
            serde_json::from_str(&render_json(&target(), &files(), &mut annotations)).unwrap();
        let file = &json["files"][0];
        let file_comments = file["file_comments"].as_array().unwrap();
        assert_eq!(file_comments.len(), 1);
        assert_eq!(file_comments[0]["text"], "this file needs tests");
        assert!(
            file_comments[0].get("side").is_none() && file_comments[0].get("start").is_none(),
            "file comments carry no line numbers"
        );
        assert_eq!(file["comments"].as_array().unwrap().len(), 1);
        assert_eq!(file["comments"][0]["side"], "new");
    }

    #[test]
    fn plain_text_renders_a_whole_file_comment_without_line_numbers() {
        let mut annotations = vec![
            file_comment(0, "this file needs tests\nand a doc comment"),
            annotation(0, 12, 14, Side::New, Body::Comment("line comment".into())),
        ];
        let text = render_text(&target(), &files(), &mut annotations);
        assert!(
            text.contains("  whole file: this file needs tests"),
            "{text}"
        );
        assert!(text.contains("    | and a doc comment"), "{text}");
        assert!(
            text.find("whole file:").unwrap() < text.find("L12-14:").unwrap(),
            "file comments lead their file section:\n{text}"
        );
    }

    #[test]
    fn whole_file_comments_are_the_only_file_scoped_kind() {
        assert!(crate::model::file_anchor(0, Body::Comment("c".into())).is_ok());
        assert!(crate::model::file_anchor(0, Body::Edit("e".into())).is_err());
        assert!(crate::model::file_anchor(0, Body::Delete).is_err());
    }
}
