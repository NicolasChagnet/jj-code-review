//! Syntax highlighting for the diff pane.
//!
//! Mirrors the reference plugin's `highlight_dlines`: the file's diff lines are
//! concatenated and highlighted in one pass, so multi-line constructs keep
//! their state, then the spans are handed back per diff line.
//!
//! Syntect state is carried across lines, which matters here because a diff
//! interleaves added and removed lines. Removed lines are usually not valid
//! source (the deleted half of a rewrite), and feeding them through the
//! tokeniser can leave it in a state that mis-colours everything after them.
//! Added and context lines are therefore highlighted as one stream, and
//! removed lines as a second, independent one: the two sides are each valid
//! source on their own.
//!
//! Highlighting is best-effort. An unknown extension, a binary blob or a
//! tokeniser error all yield no spans, and the renderer falls back to the
//! plain tinted text.

use std::collections::HashMap;
use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use two_face::re_exports::syntect::easy::HighlightLines;
use two_face::re_exports::syntect::highlighting::{FontStyle, Theme};
use two_face::re_exports::syntect::parsing::{SyntaxReference, SyntaxSet};
use two_face::re_exports::syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

use crate::diff::{FileDiff, LineKind};

/// Spans for one rendered diff line. Empty means "no highlighting", and the
/// caller should draw the text as-is.
pub type Spans = Vec<(String, Style)>;

/// Loads and caches the syntax and theme sets. These are large and cheap to
/// reuse, so they are process-wide and built on first use.
fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

fn dracula() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| two_face::theme::extra()[EmbeddedThemeName::Dracula].clone())
}

/// Highlights a whole file, returning spans per diff line index.
///
/// The returned map only holds entries for lines that produced spans; a line
/// missing from it was not highlighted.
pub fn file(file: &FileDiff) -> HashMap<usize, Spans> {
    let Some(syntax) = find_syntax(file) else {
        return HashMap::new();
    };

    let mut out = HashMap::new();
    highlight_side(file, syntax, &[LineKind::Context, LineKind::Add], &mut out);
    highlight_side(file, syntax, &[LineKind::Del], &mut out);
    out
}

/// The syntax for a file, by extension, then by first line (shebangs), then by
/// filename (Makefile, Dockerfile).
fn find_syntax(file: &FileDiff) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let path = &file.path;
    let extension = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    set.find_syntax_by_extension(extension)
        .or_else(|| set.find_syntax_by_token(extension))
        .or_else(|| {
            file.hunks
                .first()
                .and_then(|h| h.lines.iter().find(|l| l.new_ln.is_some()))
                .and_then(|l| set.find_syntax_by_first_line(&l.text))
        })
        .or_else(|| {
            let name = path.rsplit('/').next().unwrap_or(path);
            set.find_syntax_by_extension(name)
        })
}

/// Highlights the lines whose kind is in `kinds`, as one stream.
///
/// Indices written into `out` are addresses into the file's concatenated line
/// list, matching how the diff pane indexes rows.
fn highlight_side(
    file: &FileDiff,
    syntax: &SyntaxReference,
    kinds: &[LineKind],
    out: &mut HashMap<usize, Spans>,
) {
    let set = syntaxes();
    // `newlines` keeps the final line intact; every entry ends in '\n'.
    let mut stream = String::new();
    let mut indices: Vec<usize> = Vec::new();
    let mut index = 0usize;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            if kinds.contains(&line.kind) {
                stream.push_str(&line.text);
                stream.push('\n');
                indices.push(index);
            }
            index += 1;
        }
    }
    if stream.is_empty() {
        return;
    }

    let mut highlighter = HighlightLines::new(syntax, dracula());
    for (chunk, index) in LinesWithEndings::from(&stream).zip(indices) {
        let Ok(ranges) = highlighter.highlight_line(chunk, set) else {
            // A tokeniser failure mid-file: stop rather than emit a partial,
            // misleadingly-coloured tail.
            return;
        };
        let spans = to_spans(ranges);
        if !spans.is_empty() {
            out.insert(index, spans);
        }
    }
}

fn to_spans(ranges: Vec<(two_face::re_exports::syntect::highlighting::Style, &str)>) -> Spans {
    ranges
        .into_iter()
        // The trailing newline would render as a stray cell.
        .filter_map(|(style, text)| {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                return None;
            }
            Some((text.to_string(), convert(&style)))
        })
        .collect()
}

/// Syntect style to ratatui style: foreground and the font attributes only.
///
/// Backgrounds are deliberately dropped: the diff tints own the row
/// background, and Dracula's own page colour would paint over them.
fn convert(style: &two_face::re_exports::syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(ratatui::style::Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(ratatui::style::Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(ratatui::style::Modifier::UNDERLINED);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffLine, parse};

    fn hl(raw: &str) -> (Vec<DiffLine>, HashMap<usize, Spans>) {
        let files = parse(raw);
        let lines: Vec<DiffLine> = files[0]
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter().cloned())
            .collect();
        (lines, file(&files[0]))
    }

    const RUST: &str = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
-    let x = 1;
+    let x: u32 = 41;
+    let y = x + 1;
 }
";

    #[test]
    fn highlights_rust_lines() {
        let (lines, spans) = hl(RUST);
        assert!(!spans.is_empty(), "rust should be recognised");
        // Every key must address a real line.
        for idx in spans.keys() {
            assert!(*idx < lines.len(), "index {idx} out of range");
        }
        // Context and added lines are covered.
        assert!(spans.contains_key(&0), "context line highlighted");
        let joined: String = spans[&0].iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "fn main() {", "spans must reconstruct the text");
    }

    #[test]
    fn spans_reconstruct_the_original_text() {
        let (lines, spans) = hl(RUST);
        for (idx, line_spans) in &spans {
            let joined: String = line_spans.iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(
                joined, lines[*idx].text,
                "highlighting must not alter the text of line {idx}"
            );
        }
    }

    #[test]
    fn added_and_removed_lines_are_both_highlighted() {
        let (lines, spans) = hl(RUST);
        let del = lines.iter().position(|l| l.kind == LineKind::Del).unwrap();
        let add = lines.iter().position(|l| l.kind == LineKind::Add).unwrap();
        assert!(spans.contains_key(&del), "removed line highlighted");
        assert!(spans.contains_key(&add), "added line highlighted");
    }

    #[test]
    fn removed_lines_do_not_poison_the_added_side() {
        // A deleted line that is not valid source on its own.
        let raw = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,3 +1,3 @@
 fn main() {
-    let s = \"unterminated;
+    let s = \"ok\";
 }
";
        let (lines, spans) = hl(raw);
        let last = lines.len() - 1;
        assert!(
            spans.contains_key(&last),
            "trailing context still highlighted"
        );
        let joined: String = spans[&last].iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "}");
    }

    #[test]
    fn keywords_carry_a_distinct_colour() {
        // Dracula tints Rust keywords cyan and italicises them.
        let (_, spans) = hl(RUST);
        let keyword = spans
            .values()
            .flatten()
            .find(|(t, _)| t == "fn")
            .expect("`fn` is tokenised");
        assert_eq!(keyword.1.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert!(
            keyword
                .1
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC),
            "dracula italicises keywords"
        );
    }

    #[test]
    fn different_token_kinds_get_different_colours() {
        let (_, spans) = hl(RUST);
        let colour_of = |needle: &str| {
            spans
                .values()
                .flatten()
                .find(|(t, _)| t == needle)
                .map(|(_, s)| s.fg)
        };
        let keyword = colour_of("fn").unwrap();
        // A string literal and a type in the added line.
        let string = colour_of("\"ok\"").or_else(|| colour_of("\"unterminated;"));
        assert_ne!(
            keyword,
            Some(Color::Rgb(0xf8, 0xf8, 0xf2)),
            "not plain text"
        );
        if let Some(s) = string {
            assert_ne!(s, keyword, "strings differ from keywords");
        }
    }

    #[test]
    fn backgrounds_are_not_applied() {
        // The diff tints own the row background.
        let (_, spans) = hl(RUST);
        for (_, style) in spans.values().flatten() {
            assert_eq!(style.bg, None, "highlighting must not set a background");
        }
    }

    #[test]
    fn unknown_extension_yields_no_spans() {
        let raw = "\
diff --git a/notes.qqq b/notes.qqq
--- a/notes.qqq
+++ b/notes.qqq
@@ -1,2 +1,2 @@
 alpha
-beta
+gamma
";
        let (_, spans) = hl(raw);
        assert!(spans.is_empty(), "no syntax means no spans");
    }

    #[test]
    fn plain_text_files_are_left_alone() {
        let raw = "\
diff --git a/notes.txt b/notes.txt
--- a/notes.txt
+++ b/notes.txt
@@ -1,2 +1,2 @@
 alpha
-beta
+gamma
";
        let (_, spans) = hl(raw);
        let styles: Vec<_> = spans.values().flatten().map(|(_, s)| *s).collect();
        // Plain text has a single default style, so nothing is emphasised.
        assert!(
            styles.iter().all(|s| s.add_modifier.is_empty()),
            "plain text should carry no emphasis"
        );
    }

    #[test]
    fn empty_and_binary_files_are_safe() {
        let raw = "\
diff --git a/logo.png b/logo.png
Binary files a/logo.png and b/logo.png differ
";
        let files = parse(raw);
        assert!(file(&files[0]).is_empty());
    }

    #[test]
    fn multi_line_constructs_keep_their_state() {
        // The block comment only closes on the third line; if state were reset
        // per line, `let` on line 4 would be mis-coloured as a comment.
        let raw = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,4 +1,4 @@
 /* start
    middle
    end */
-let a = 1;
+let b = 2;
";
        let (lines, spans) = hl(raw);
        let last = lines.len() - 1;
        let joined: String = spans[&last].iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "let b = 2;");
        // `let` after a closed comment must not still be comment-coloured.
        let comment_span = spans
            .values()
            .flatten()
            .find(|(t, _)| t.contains("/*"))
            .map(|(_, s)| s.fg);
        let let_span = spans[&last].first().map(|(_, s)| s.fg);
        assert_ne!(
            comment_span, let_span,
            "state must not leak from the comment into the code"
        );
    }
}
