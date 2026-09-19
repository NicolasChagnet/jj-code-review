//! The review TUI: app state, key handling and rendering.
//!
//! Layout is a left sidebar with two stacked panes (Files, Comments) and a
//! right pane holding the current file's diff. `Tab` cycles focus. Navigation
//! (`n`/`p`, `[`/`]`, `v`, `c`/`e`/`x`/`d`, `?`, `s`, `q`) works from every
//! pane; the remaining keys are pane-scoped.
//!
//! A file's diff is one flat vector of [`Row`]s: hunk separators and content
//! rows are both single entries. The diff cursor is an index into that vector
//! and movement walks over separators, so `]`/`[` and `j`/`k` share one model.
//! Annotations address content rows (separators excluded) because that is what
//! [`crate::model::anchor`] expects.
//!
//! The diff is snapshotted at launch and never refreshed (v1).

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::diff::{DiffLine, FileDiff, LineKind, Status};
use crate::input::TextInput;
use crate::jj::Target;
use crate::model::{self, AnchorError, Annotation, Body, Kind};
use crate::theme::Theme;

const SIDEBAR_WIDTH: u16 = 28;
const PAGE: usize = 20;

/// Which pane receives keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Diff,
    Files,
    Comments,
}

/// One rendered row of a file's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Hunk(String),
    Line(usize),
}

/// Transient status-bar message, counted down in frames.
struct Flash {
    text: String,
    frames: u8,
}

/// The comment/edit entry box, docked under the diff pane.
struct Popup {
    input: TextInput,
    /// Which file the annotation belongs to. Captured on open so switching
    /// files (via the Files pane, `n`/`p`) cannot retarget it.
    file: usize,
    /// Range to anchor on save; `None` when editing an existing annotation.
    range: Option<(usize, usize)>,
    /// Index into `annotations` when this box edits in place.
    existing: Option<usize>,
    kind: Kind,
    /// Whole-file comment: no line range, no side.
    file_scope: bool,
    title: String,
}

pub struct App {
    target: Target,
    files: Vec<FileDiff>,
    /// Content rows of each file (separators excluded), indexed by annotation.
    content: Vec<Vec<DiffLine>>,
    /// All rendered rows per file, separators included.
    rows: Vec<Vec<Row>>,
    annotations: Vec<Annotation>,
    /// Set when the review range is empty, replacing the diff pane.
    empty_state: Option<String>,

    focus: Focus,
    file: usize,
    /// Diff cursor per file, remembered across file switches.
    cursors: Vec<usize>,
    comment_cursor: usize,
    vstart: Option<usize>,
    popup: Option<Popup>,
    flash: Option<Flash>,
    theme: Theme,
    help: bool,
    /// `Some(true)` on submit, `Some(false)` when the user quits.
    outcome: Option<bool>,
}

impl App {
    pub fn new(target: Target, files: Vec<FileDiff>, empty_state: Option<String>) -> Self {
        let content: Vec<Vec<DiffLine>> = files
            .iter()
            .map(|f| {
                f.hunks
                    .iter()
                    .flat_map(|h| h.lines.iter().cloned())
                    .collect()
            })
            .collect();
        let rows: Vec<Vec<Row>> = files
            .iter()
            .map(|f| {
                let mut rows = Vec::new();
                for hunk in &f.hunks {
                    rows.push(Row::Hunk(hunk.header.clone()));
                    rows.extend((0..hunk.lines.len()).map(Row::Line));
                }
                rows
            })
            .collect();
        let cursors = rows.iter().map(|r| first_content_row(r)).collect();
        let empty_state = empty_state.or_else(|| {
            files
                .iter()
                .all(|f| f.hunks.is_empty() && !f.binary)
                .then(|| "No changes in this range.".to_string())
        });
        Self {
            target,
            files,
            content,
            rows,
            annotations: Vec::new(),
            empty_state,
            focus: Focus::Diff,
            file: 0,
            cursors,
            comment_cursor: 0,
            vstart: None,
            popup: None,
            flash: None,
            theme: Theme::from_env(),
            help: false,
            outcome: None,
        }
    }

    // --- model access ------------------------------------------------------

    /// The resolved review target.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The parsed diff, in file order.
    pub fn files(&self) -> &[FileDiff] {
        &self.files
    }

    /// Consumes the collected annotations, for serialization on submit.
    pub fn take_annotations(&mut self) -> Vec<Annotation> {
        std::mem::take(&mut self.annotations)
    }

    fn rows(&self) -> &[Row] {
        self.rows.get(self.file).map(Vec::as_slice).unwrap_or(&[])
    }

    fn lines(&self, file: usize) -> &[DiffLine] {
        self.content.get(file).map(Vec::as_slice).unwrap_or(&[])
    }

    fn is_binary(&self) -> bool {
        self.files.get(self.file).is_some_and(|f| f.binary)
    }

    fn cursor(&self) -> usize {
        self.cursors.get(self.file).copied().unwrap_or(0)
    }

    fn set_cursor(&mut self, row: usize) {
        if let Some(slot) = self.cursors.get_mut(self.file) {
            *slot = row;
        }
    }

    /// Diff row index holding content row `content_idx` of the current file.
    fn row_of_content(&self, content_idx: usize) -> usize {
        self.rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, Row::Line(_)))
            .nth(content_idx)
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Content-row index of a diff row.
    fn content_of_row(&self, row: usize) -> usize {
        self.content_index_in(self.file, row)
    }

    /// Content-row index of a diff row within `file`, which may not be the
    /// current one while an annotation box is open for another file.
    fn content_index_in(&self, file: usize, row: usize) -> usize {
        self.rows
            .get(file)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .take(row + 1)
            .filter(|r| matches!(r, Row::Line(_)))
            .count()
            .saturating_sub(1)
    }

    fn line_at_content(&self, li: usize) -> Option<&DiffLine> {
        self.lines(self.file).get(li)
    }

    // --- navigation --------------------------------------------------------

    /// Moves the diff cursor by `delta` content rows, skipping separators.
    fn move_cursor(&mut self, delta: isize) {
        let rows = self.rows().to_vec();
        if rows.is_empty() {
            return;
        }
        let step: isize = if delta > 0 { 1 } else { -1 };
        let mut idx = self.cursor();
        for _ in 0..delta.unsigned_abs() {
            let mut next = idx as isize + step;
            while next >= 0
                && (next as usize) < rows.len()
                && matches!(rows[next as usize], Row::Hunk(_))
            {
                next += step;
            }
            if next < 0 || next as usize >= rows.len() {
                break;
            }
            idx = next as usize;
        }
        self.set_cursor(idx);
    }

    fn jump_to_first(&mut self) {
        self.set_cursor(first_content_row(self.rows()));
    }

    fn jump_to_last(&mut self) {
        self.set_cursor(last_content_row(self.rows()));
    }

    /// `]`: the next hunk start, wrapping to the first hunk of this file.
    fn next_hunk(&mut self) {
        let rows = self.rows().to_vec();
        let cur = self.cursor();
        if last_content_row(&rows) != cur
            && let Some(h) = (cur + 1..rows.len()).find(|&i| matches!(rows[i], Row::Hunk(_)))
        {
            self.set_cursor(h + first_content_row(&rows[h..]));
            return;
        }
        // Wrap around within this file.
        self.jump_to_first();
    }

    /// `[`: the previous hunk start, wrapping to the last hunk of this file.
    fn prev_hunk(&mut self) {
        let rows = self.rows().to_vec();
        let cur = self.cursor();
        if first_content_row(&rows) != cur
            && let Some(h) = (0..cur).rev().find(|&i| matches!(rows[i], Row::Hunk(_)))
        {
            self.set_cursor(h + first_content_row(&rows[h..]));
            return;
        }
        // Wrap around to the start of this file's last hunk.
        match (0..rows.len())
            .rev()
            .find(|&i| matches!(rows[i], Row::Hunk(_)))
        {
            Some(h) => self.set_cursor(h + first_content_row(&rows[h..])),
            None => self.jump_to_last(),
        }
    }

    fn select_file(&mut self, file: usize) {
        if file >= self.files.len() {
            return;
        }
        self.file = file;
        self.vstart = None;
    }

    fn next_file(&mut self) {
        if self.file + 1 < self.files.len() {
            self.select_file(self.file + 1);
        }
    }

    fn prev_file(&mut self) {
        if self.file > 0 {
            self.select_file(self.file - 1);
        }
    }

    /// Selection range in diff-row indexes, when `v` is active.
    fn selection(&self) -> Option<(usize, usize)> {
        let start = self.vstart?;
        let cur = self.cursor();
        Some((start.min(cur), start.max(cur)))
    }

    /// The rows an action applies to: the selection, else the cursor row.
    fn action_range(&self) -> Option<(usize, usize)> {
        if let Some(range) = self.selection() {
            return Some(range);
        }
        let row = self.cursor();
        matches!(self.rows().get(row), Some(Row::Line(_))).then_some((row, row))
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some(Flash {
            text: text.into(),
            frames: 24,
        });
    }

    // --- actions -----------------------------------------------------------

    fn open_popup(&mut self, kind: Kind) {
        let Some((from, to)) = self.action_range() else {
            self.flash("Move onto a diff line first (j/k)");
            return;
        };
        let lines = self.lines(self.file).to_vec();
        let (cf, ct) = (self.content_of_row(from), self.content_of_row(to));

        // Re-opening on the same range edits the existing annotation.
        if let Some(idx) = self.find_covering(kind, cf, ct) {
            let body = match &self.annotations[idx].body {
                Body::Comment(t) | Body::Edit(t) => t.clone(),
                Body::Delete => String::new(),
            };
            self.start_box(kind, body, Some(idx), None, false);
            return;
        }

        let probe = body_for(kind, String::new());
        match model::anchor(self.file, &lines, cf, ct, probe) {
            Ok(_) => {
                let prefill = if kind == Kind::Edit {
                    model::new_side_content(&lines, cf, ct)
                } else {
                    String::new()
                };
                self.start_box(kind, prefill, None, Some((from, to)), false);
            }
            Err(e) => self.flash(anchor_message(e)),
        }
    }

    /// `c` from the Files pane: comment on the file as a whole.
    fn open_file_comment(&mut self) {
        let file = self.file;
        if let Some(idx) = self
            .annotations
            .iter()
            .position(|a| a.file == file && a.is_file_scope() && a.kind() == Kind::Comment)
        {
            let text = match &self.annotations[idx].body {
                Body::Comment(t) => t.clone(),
                _ => String::new(),
            };
            self.start_box(Kind::Comment, text, Some(idx), None, true);
            return;
        }
        self.start_box(Kind::Comment, String::new(), None, None, true);
    }

    fn start_box(
        &mut self,
        kind: Kind,
        text: String,
        existing: Option<usize>,
        range: Option<(usize, usize)>,
        file_scope: bool,
    ) {
        let file = self.file;
        let title = match (file_scope, existing) {
            (true, _) => format!("{} on this file", kind_title(kind)),
            (false, Some(idx)) => {
                format!("{} on {}", kind_title(kind), self.annotations[idx].label())
            }
            (false, None) => {
                let lines = self.lines(file).to_vec();
                let (cf, ct) = range
                    .map(|(f, t)| (self.content_of_row(f), self.content_of_row(t)))
                    .unwrap_or((0, 0));
                match model::anchor(file, &lines, cf, ct, body_for(kind, String::new())) {
                    Ok(a) => format!("{} on {}", kind_title(kind), a.label()),
                    Err(_) => kind_title(kind).to_string(),
                }
            }
        };
        self.popup = Some(Popup {
            input: TextInput::from(&text),
            file,
            range,
            existing,
            kind,
            file_scope,
            title,
        });
        self.vstart = None;
    }

    /// An existing annotation of `kind` covering exactly this content range.
    fn find_covering(&self, kind: Kind, cf: usize, ct: usize) -> Option<usize> {
        self.annotations.iter().position(|a| {
            !a.is_file_scope()
                && a.kind() == kind
                && a.file == self.file
                && a.anchor_row == cf
                && a.max_row() == ct
        })
    }

    fn save_box(&mut self) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        let text = popup.input.value().trim_end().to_string();
        if text.trim().is_empty() {
            self.flash("Empty text discarded");
            return;
        }
        if let Some(idx) = popup.existing {
            if let Some(a) = self.annotations.get_mut(idx) {
                match &mut a.body {
                    Body::Comment(t) | Body::Edit(t) => *t = text,
                    Body::Delete => {}
                }
            }
            return;
        }

        let file = popup.file;
        let body = body_for(popup.kind, text);
        if popup.file_scope {
            match model::file_anchor(file, body) {
                Ok(a) => self.annotations.push(a),
                Err(e) => self.flash(anchor_message(e)),
            }
            return;
        }

        let Some((from, to)) = popup.range else {
            return;
        };
        let lines = self.lines(file).to_vec();
        let (cf, ct) = (
            self.content_index_in(file, from),
            self.content_index_in(file, to),
        );
        match model::anchor(file, &lines, cf, ct, body) {
            Ok(a) => {
                if popup.kind == Kind::Edit
                    && a_text(&a).trim_end() == model::new_side_content(&lines, cf, ct).trim_end()
                {
                    self.flash("Edit discarded: identical to the original");
                    return;
                }
                self.annotations.push(a);
            }
            Err(e) => self.flash(anchor_message(e)),
        }
    }

    /// `x`: mark the range for deletion (replacing any deletion over it).
    fn mark_delete(&mut self) {
        let Some((from, to)) = self.action_range() else {
            self.flash("Move onto a diff line first (j/k)");
            return;
        };
        let lines = self.lines(self.file).to_vec();
        let (cf, ct) = (self.content_of_row(from), self.content_of_row(to));
        match model::anchor(self.file, &lines, cf, ct, Body::Delete) {
            Ok(a) => {
                self.annotations.retain(|x| {
                    !(x.file == a.file && x.kind() == Kind::Delete && x.anchor_row == a.anchor_row)
                });
                self.annotations.push(a);
                self.vstart = None;
                self.flash("Marked for deletion");
            }
            Err(e) => self.flash(anchor_message(e)),
        }
    }

    /// `d` in the diff: drop every annotation anchored at the cursor row.
    fn clear_at_cursor(&mut self) {
        let row = self.cursor();
        let li = self.content_of_row(row);
        let before = self.annotations.len();
        self.annotations
            .retain(|a| !(a.file == self.file && !a.is_file_scope() && a.anchor_row == li));
        if self.annotations.len() == before {
            self.flash("No annotation here");
        } else {
            self.flash("Annotations cleared");
        }
    }

    /// `d` in the Files pane: drop every annotation on the selected file.
    fn clear_file_annotations(&mut self) {
        let file = self.file;
        let before = self.annotations.len();
        self.annotations.retain(|a| a.file != file);
        if self.annotations.len() == before {
            self.flash("No annotations on this file");
        } else {
            self.flash("File annotations cleared");
        }
    }

    fn delete_selected_annotation(&mut self) {
        let ordered = self.ordered_annotations();
        let Some(&idx) = ordered.get(self.comment_cursor) else {
            return;
        };
        self.annotations.remove(idx);
        self.flash("Annotation deleted");
        self.comment_cursor = self
            .comment_cursor
            .min(self.annotations.len().saturating_sub(1));
    }

    /// Annotation indexes in submit order, which is also the Comments order.
    fn ordered_annotations(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.annotations.len()).collect();
        idx.sort_by_key(|&i| {
            let a = &self.annotations[i];
            (a.file, a.start, a.end, a.kind().rank())
        });
        idx
    }

    /// `Enter` / `g` in the Comments pane.
    fn goto_selected_annotation(&mut self) {
        let ordered = self.ordered_annotations();
        let Some(&idx) = ordered.get(self.comment_cursor) else {
            return;
        };
        let (file, content) = {
            let a = &self.annotations[idx];
            (a.file, a.anchor_row)
        };
        self.select_file(file);
        let row = self.row_of_content(content);
        self.set_cursor(row);
        self.focus = Focus::Diff;
    }

    fn submit(&mut self) {
        self.outcome = Some(true);
    }

    fn quit(&mut self) {
        self.outcome = Some(false);
    }

    fn cycle_focus(&mut self, back: bool) {
        self.focus = match (self.focus, back) {
            (Focus::Diff, false) => Focus::Files,
            (Focus::Files, false) => Focus::Comments,
            (Focus::Comments, false) => Focus::Diff,
            (Focus::Diff, true) => Focus::Comments,
            (Focus::Comments, true) => Focus::Files,
            (Focus::Files, true) => Focus::Diff,
        };
    }

    // --- keys --------------------------------------------------------------

    /// Handles one key event. Returns true when the app is done.
    pub fn handle_key(&mut self, ev: KeyEvent) -> bool {
        if ev.kind == KeyEventKind::Release {
            return false;
        }
        if let Some(flash) = &mut self.flash {
            flash.frames = flash.frames.saturating_sub(1);
            if flash.frames == 0 {
                self.flash = None;
            }
        }
        if self.help {
            self.help = false;
            return false;
        }
        if self.popup.is_some() {
            self.popup_key(ev);
            return false;
        }

        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(ev.code, KeyCode::Char('c')) {
            self.quit();
            return true;
        }

        match ev.code {
            KeyCode::Char('s') if !ctrl => {
                self.submit();
                return true;
            }
            KeyCode::Char('q') if !ctrl => {
                self.quit();
                return true;
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab => self.cycle_focus(false),
            KeyCode::BackTab => self.cycle_focus(true),
            KeyCode::Char('n') if !ctrl => self.next_file(),
            KeyCode::Char('p') if !ctrl => self.prev_file(),
            KeyCode::Char(']') => {
                self.next_hunk();
                self.focus = Focus::Diff;
            }
            KeyCode::Char('[') => {
                self.prev_hunk();
                self.focus = Focus::Diff;
            }
            KeyCode::Char('v') if !ctrl => {
                self.vstart = if self.vstart.is_some() {
                    None
                } else {
                    Some(self.cursor())
                };
            }
            // From the Files pane, `c` comments on the whole file; the
            // line-oriented actions belong to the diff and comments panes.
            KeyCode::Char('c') if !ctrl => match self.focus {
                Focus::Files => self.open_file_comment(),
                _ => self.open_popup(Kind::Comment),
            },
            KeyCode::Char('e') if !ctrl => self.open_popup(Kind::Edit),
            KeyCode::Char('x') if !ctrl => self.mark_delete(),
            KeyCode::Char('d') if !ctrl => match self.focus {
                Focus::Comments => self.delete_selected_annotation(),
                Focus::Files => self.clear_file_annotations(),
                Focus::Diff => self.clear_at_cursor(),
            },
            KeyCode::Esc if self.vstart.is_some() => self.vstart = None,
            KeyCode::Esc => {
                self.quit();
                return true;
            }
            _ => match self.focus {
                Focus::Diff => self.diff_key(ev),
                Focus::Files => self.files_key(ev),
                Focus::Comments => self.comments_key(ev),
            },
        }
        false
    }

    /// Popup keys are handled here so the widget never sees Esc or Enter.
    fn popup_key(&mut self, ev: KeyEvent) {
        match ev.code {
            KeyCode::Esc => self.popup = None,
            KeyCode::Enter => self.save_box(),
            _ => {
                if let Some(popup) = &mut self.popup {
                    popup.input.handle_key(&ev);
                }
            }
        }
    }

    fn diff_key(&mut self, ev: KeyEvent) {
        match ev.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::PageDown => self.move_cursor(PAGE as isize),
            KeyCode::PageUp => self.move_cursor(-(PAGE as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.jump_to_first(),
            KeyCode::Char('G') | KeyCode::End => self.jump_to_last(),
            _ => {}
        }
    }

    fn files_key(&mut self, ev: KeyEvent) {
        match ev.code {
            KeyCode::Char('j') | KeyCode::Down => self.next_file(),
            KeyCode::Char('k') | KeyCode::Up => self.prev_file(),
            KeyCode::Char('g') | KeyCode::Home => self.select_file(0),
            KeyCode::Char('G') | KeyCode::End => {
                self.select_file(self.files.len().saturating_sub(1))
            }
            KeyCode::Enter => self.focus = Focus::Diff,
            _ => {}
        }
    }

    fn comments_key(&mut self, ev: KeyEvent) {
        let len = self.annotations.len();
        match ev.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.comment_cursor + 1 < len {
                    self.comment_cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.comment_cursor = self.comment_cursor.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => self.comment_cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.comment_cursor = len.saturating_sub(1),
            KeyCode::Enter => self.goto_selected_annotation(),
            _ => {}
        }
    }

    // --- rendering ---------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let cols = Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(20)])
            .split(area);
        let sidebar = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(cols[0]);
        // The entry box is docked under the diff, above the hint and status
        // rows, so it never covers the code being reviewed.
        let input_height = self.input_height();
        let body = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(cols[1]);

        self.draw_files(frame, sidebar[0]);
        self.draw_comments(frame, sidebar[1]);
        self.draw_diff(frame, body[0]);
        if input_height > 0 {
            self.draw_input(frame, body[1]);
        }
        self.draw_keys(frame, body[2]);
        self.draw_status(frame, body[3]);

        if self.help {
            draw_help(frame, area, &self.theme);
        }
    }

    /// Rows reserved for the entry box: zero when it is closed. The box grows
    /// with its content, capped so the diff always keeps some room.
    fn input_height(&self) -> u16 {
        match &self.popup {
            None => 0,
            Some(popup) => (popup.input.line_count() as u16 + 2).clamp(3, 10),
        }
    }

    fn draw_files(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let (c, e, d) = counts_for(&self.annotations, i);
                let room = area.width.saturating_sub(16) as usize;
                let mut spans = vec![
                    Span::styled(
                        format!("{} ", status_letter(f)),
                        Style::default().fg(status_color(f)),
                    ),
                    Span::raw(truncate(&f.path, room)),
                ];
                if c > 0 {
                    spans.push(Span::styled(
                        format!(" ●{c}"),
                        Style::default().fg(self.theme.comment_fg),
                    ));
                }
                if e > 0 {
                    spans.push(Span::styled(
                        format!(" ✎{e}"),
                        Style::default().fg(self.theme.accent_fg),
                    ));
                }
                if d > 0 {
                    spans.push(Span::styled(
                        format!(" ✗{d}"),
                        Style::default().fg(Color::Red),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let mut state = ListState::default();
        if !self.files.is_empty() {
            state.select(Some(self.file.min(self.files.len() - 1)));
        }
        let list = List::new(items)
            .block(pane_block(
                " Files ",
                self.focus == Focus::Files,
                &self.theme,
            ))
            .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_comments(&self, frame: &mut Frame, area: Rect) {
        let ordered = self.ordered_annotations();
        let width = area.width.saturating_sub(3) as usize;
        let items: Vec<ListItem> = ordered
            .iter()
            .map(|&i| {
                let a = &self.annotations[i];
                let path = self
                    .files
                    .get(a.file)
                    .map(|f| f.path.as_str())
                    .unwrap_or("?");
                let head = format!(
                    "{} {} {} ",
                    a.kind().marker(),
                    truncate(basename(path), 12),
                    a.label()
                );
                let room = width.saturating_sub(head.chars().count());
                ListItem::new(Line::from(vec![
                    Span::styled(head, Style::default().fg(self.theme.comment_fg)),
                    Span::raw(truncate(a_text(a).lines().next().unwrap_or(""), room)),
                ]))
            })
            .collect();

        let mut state = ListState::default();
        if !ordered.is_empty() {
            state.select(Some(self.comment_cursor.min(ordered.len() - 1)));
        }
        let list = List::new(items)
            .block(pane_block(
                " Comments ",
                self.focus == Focus::Comments,
                &self.theme,
            ))
            .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_diff(&mut self, frame: &mut Frame, area: Rect) {
        let block = pane_block(" Diff ", self.focus == Focus::Diff, &self.theme);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = inner.width as usize;

        if let Some(msg) = self
            .empty_state
            .clone()
            .or_else(|| self.is_binary().then(|| "[binary file]".to_string()))
        {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(Color::DarkGray),
                ))),
                inner,
            );
            return;
        }

        let (lines, cursor_line) = self.diff_lines(width);
        let height = inner.height as usize;
        let offset = cursor_line.saturating_sub(height.saturating_sub(1));
        frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), inner);
    }

    /// Renders the current file's diff; also returns the screen line holding
    /// the cursor, so the pane can keep it visible.
    fn diff_lines(&self, width: usize) -> (Vec<Line<'static>>, usize) {
        let rows = self.rows();
        let selection = self.selection();
        let cursor = self.cursor();
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut cursor_line = 0;

        for (idx, row) in rows.iter().enumerate() {
            match row {
                Row::Hunk(header) => out.push(Line::from(Span::styled(
                    truncate(&format!(" {header}"), width),
                    Style::default().fg(self.theme.accent_fg),
                ))),
                Row::Line(li) => {
                    let Some(line) = self.line_at_content(*li) else {
                        continue;
                    };
                    let selected = selection.is_some_and(|(f, t)| idx >= f && idx <= t);
                    let annotation = annotation_at(&self.annotations, self.file, *li, line);
                    let bg = if selected {
                        self.theme.sel_bg
                    } else if annotation.is_some() {
                        self.theme.com_bg
                    } else {
                        self.theme.tint(line.kind)
                    };
                    let base = bg_style(bg);

                    let (sign, sign_fg) = match line.kind {
                        LineKind::Add => ("+", Color::Green),
                        LineKind::Del => ("-", Color::Red),
                        LineKind::Context => (" ", self.theme.dim_fg),
                    };
                    let mark = annotation.map(|a| a.kind().marker()).unwrap_or(" ");
                    let text_style = if annotation.is_some_and(|a| a.kind() == Kind::Delete) {
                        base.add_modifier(Modifier::CROSSED_OUT)
                    } else {
                        base
                    };

                    // 1 mark + 1 space + two 4-wide gutters + 1 space
                    // + 1 sign + 1 space
                    let text_width = width.saturating_sub(14);
                    let mut text = line.text.clone();
                    if line.text.chars().count() > text_width {
                        text = line
                            .text
                            .chars()
                            .take(text_width.saturating_sub(1))
                            .collect();
                        text.push('…');
                    }
                    // Selection is signalled by weight as well as tint: a
                    // blended background alone is too subtle to read on some
                    // themes, and the tint can vanish against a light one.
                    let emphasis = if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    };
                    let spans = vec![
                        Span::styled(
                            format!("{mark} "),
                            base.fg(self.theme.comment_fg).add_modifier(emphasis),
                        ),
                        // Both gutters are always drawn: showing only the
                        // side a row belongs to makes a replacement read as
                        // two rows with repeated, out-of-order numbers.
                        Span::styled(
                            format!("{:>4} ", number_or_blank(line.old_ln)),
                            base.fg(self.theme.dim_fg).add_modifier(emphasis),
                        ),
                        Span::styled(
                            format!("{:>4} ", number_or_blank(line.new_ln)),
                            base.fg(self.theme.dim_fg).add_modifier(emphasis),
                        ),
                        Span::styled(
                            format!("{sign} "),
                            base.fg(if selected {
                                self.theme.accent_fg
                            } else {
                                sign_fg
                            })
                            .add_modifier(emphasis),
                        ),
                        Span::styled(text, text_style.add_modifier(emphasis)),
                    ];

                    // The cursor row and the selection must not disagree about
                    // the band: whoever wins owns the padding too, otherwise the
                    // row ends in a mismatched tail.
                    let row_bg = if selected {
                        bg
                    } else if idx == cursor {
                        self.theme.cursor_bg
                    } else {
                        bg
                    };
                    if idx == cursor {
                        cursor_line = out.len();
                    }
                    let mut rendered = Line::from(spans);
                    if let Some(bg) = row_bg {
                        rendered = pad_line(rendered, width, Some(bg));
                    }
                    out.push(rendered);

                    if let Some(a) = annotation.filter(|a| a.max_row() == *li) {
                        for l in annotation_rows(a, width, &self.theme) {
                            out.push(l);
                        }
                    }
                }
            }
        }
        (out, cursor_line)
    }

    /// Contextual key hints directly under the diff pane.
    ///
    /// Lists what the focused pane's keys do right now: the selection-aware
    /// actions change when `v` is active, and each pane gets its own movement
    /// keys.
    fn draw_keys(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 {
            return;
        }
        let mut spans = vec![Span::raw(" ")];
        let hints = self.context_hints();
        let mut first = true;
        let mut used = 1usize;
        let width = area.width as usize;
        for (key, label) in hints {
            // Rough cost: key + separator + label + spacing.
            let cost = key.chars().count() + label.chars().count() + 4;
            if !first && used + cost > width {
                break;
            }
            if !first {
                spans.push(Span::styled(" · ", Style::default().fg(self.theme.dim_fg)));
                used += 3;
            }
            spans.push(Span::styled(
                key,
                Style::default()
                    .fg(self.theme.comment_fg)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label, Style::default().fg(self.theme.dim_fg)));
            used += cost;
            first = false;
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(bg_style(self.theme.bar_bg)),
            area,
        );
    }

    /// The key/action pairs relevant to the current focus and state.
    fn context_hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.popup.is_some() {
            return vec![
                ("Enter", "save"),
                ("Esc", "cancel"),
                ("Ctrl+J", "newline"),
                ("Ctrl+W", "word"),
                ("Ctrl+K", "kill"),
            ];
        }
        if self.help {
            return vec![("any key", "close help")];
        }
        match self.focus {
            Focus::Diff => {
                let mut hints = vec![("j/k", "line"), ("] / [", "hunk"), ("g/G", "top/end")];
                if self.selection().is_some() {
                    hints.push(("v", "cancel selection"));
                    hints.push(("c", "comment range"));
                    hints.push(("e", "edit range"));
                    hints.push(("x", "delete range"));
                } else {
                    hints.push(("v", "select"));
                    hints.push(("c", "comment"));
                    hints.push(("e", "edit"));
                    hints.push(("x", "delete"));
                }
                hints.push(("d", "clear here"));
                hints.push(("n/p", "file"));
                hints.push(("Tab", "pane"));
                hints.push(("s", "submit"));
                hints.push(("q", "quit"));
                hints.push(("?", "help"));
                hints
            }
            Focus::Files => vec![
                ("j/k", "file"),
                ("Enter", "open"),
                ("n/p", "file"),
                ("c", "comment file"),
                ("d", "clear file"),
                ("e/x", "line actions"),
                ("Tab", "pane"),
                ("s", "submit"),
                ("q", "quit"),
                ("?", "help"),
            ],
            Focus::Comments => vec![
                ("j/k", "annotation"),
                ("g/G", "first/last"),
                ("Enter", "goto"),
                ("d", "delete"),
                ("c/e/x", "annotate"),
                ("Tab", "pane"),
                ("s", "submit"),
                ("q", "quit"),
                ("?", "help"),
            ],
        }
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let desc = self.target.head.short_description();
        let mut spans = vec![
            Span::styled(
                format!(" {} ", self.target.head.short()),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                if desc.is_empty() {
                    String::new()
                } else {
                    format!("\"{desc}\" ")
                },
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                format!(
                    "{}..{} ",
                    self.target.base.short(),
                    self.target.head.short()
                ),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                match self.focus {
                    Focus::Diff => "[diff] ",
                    Focus::Files => "[files] ",
                    Focus::Comments => "[comments] ",
                },
                Style::default().fg(self.theme.accent_fg),
            ),
        ];
        if let Some(flash) = &self.flash {
            spans.push(Span::styled(
                format!(" {}", flash.text),
                Style::default().fg(self.theme.comment_fg),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(bg_style(self.theme.bar_bg)),
            area,
        );
    }

    /// The docked comment/edit entry box.
    fn draw_input(&mut self, frame: &mut Frame, area: Rect) {
        let Some(popup) = &self.popup else {
            return;
        };
        popup
            .input
            .render(frame, area, &format!(" {} ", popup.title));
    }
}

// --- helpers ---------------------------------------------------------------

fn pane_block(title: &str, active: bool, theme: &Theme) -> Block<'static> {
    let style = if active {
        Style::default().fg(theme.accent_fg)
    } else {
        Style::default().fg(theme.dim_fg)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(style)
        .title(title.to_string())
}

fn draw_help(frame: &mut Frame, area: Rect, theme: &Theme) {
    let rect = centered(area, 68, 21);
    frame.render_widget(Clear, rect);
    let dim = Style::default().fg(theme.dim_fg);
    let text = vec![
        Line::from(Span::styled(
            "jcr — review keys",
            Style::default()
                .fg(theme.comment_fg)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Tab / Shift+Tab   cycle Diff · Files · Comments"),
        Line::from("j k ↑ ↓           move (per pane)"),
        Line::from("PgUp PgDn g G     page / first / last"),
        Line::from("] [               next / previous hunk (wraps in file)"),
        Line::from("n p               next / previous file"),
        Line::from("v                 visual selection (Esc cancels)"),
        Line::from("c                 comment on selection or line"),
        Line::from("e                 suggested edit (new-side lines only)"),
        Line::from("x                 mark for deletion"),
        Line::from("d                 clear here / delete selected annotation"),
        Line::from("Enter             open file · goto annotation"),
        Line::from("s                 submit and print the review"),
        Line::from("q / Esc / Ctrl-C  quit without submitting"),
        Line::from(""),
        Line::from(Span::styled(
            "Popup: Enter saves, Esc cancels, Ctrl+J inserts a newline.",
            dim,
        )),
        Line::from(Span::styled("Any key closes this help.", dim)),
    ];
    frame.render_widget(
        Paragraph::new(text).block(pane_block(" Help ", true, theme)),
        rect,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn first_content_row(rows: &[Row]) -> usize {
    rows.iter()
        .position(|r| matches!(r, Row::Line(_)))
        .unwrap_or(0)
}

fn last_content_row(rows: &[Row]) -> usize {
    rows.iter()
        .rposition(|r| matches!(r, Row::Line(_)))
        .unwrap_or(0)
}

fn bg_style(bg: Option<Color>) -> Style {
    match bg {
        Some(bg) => Style::default().bg(bg),
        None => Style::default(),
    }
}

/// The annotation covering content row `li`, if any.
fn annotation_at<'a>(
    annotations: &'a [Annotation],
    file: usize,
    li: usize,
    line: &DiffLine,
) -> Option<&'a Annotation> {
    annotations
        .iter()
        .find(|a| a.file == file && a.covers(li, line))
}

fn status_letter(f: &FileDiff) -> &'static str {
    match f.status {
        Status::Added => "A",
        Status::Deleted => "D",
        Status::Modified => "M",
        Status::Renamed => "R",
        Status::Copied => "C",
    }
}

fn status_color(f: &FileDiff) -> Color {
    match f.status {
        Status::Added => Color::Green,
        Status::Deleted => Color::Red,
        _ => Color::Yellow,
    }
}

fn counts_for(annotations: &[Annotation], file: usize) -> (usize, usize, usize) {
    let mut counts = (0, 0, 0);
    for a in annotations.iter().filter(|a| a.file == file) {
        match a.kind() {
            Kind::Comment => counts.0 += 1,
            Kind::Edit => counts.1 += 1,
            Kind::Delete => counts.2 += 1,
        }
    }
    counts
}

fn a_text(a: &Annotation) -> String {
    match &a.body {
        Body::Comment(t) | Body::Edit(t) => t.clone(),
        Body::Delete => String::new(),
    }
}

fn body_for(kind: Kind, text: String) -> Body {
    match kind {
        Kind::Comment => Body::Comment(text),
        Kind::Edit => Body::Edit(text),
        Kind::Delete => Body::Delete,
    }
}

fn kind_title(kind: Kind) -> &'static str {
    match kind {
        Kind::Comment => "Comment",
        Kind::Edit => "Edit",
        Kind::Delete => "Delete",
    }
}

fn anchor_message(e: AnchorError) -> &'static str {
    match e {
        AnchorError::Empty => "Nothing to anchor to on this row",
        AnchorError::OldSide => "Only lines that exist in the result can be changed",
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A gutter cell: the number, or blanks when the line does not exist on that
/// side (an addition has no old line, a deletion no new one).
fn number_or_blank(n: Option<u32>) -> String {
    match n {
        Some(n) => n.to_string(),
        None => String::new(),
    }
}

/// Pads a row to `width` so its background covers the whole line.
///
/// The padding inherits the row's existing modifiers, so a padded row keeps
/// whatever emphasis its spans carry (bold for a selection).
fn pad_line(line: Line<'static>, width: usize, bg: Option<Color>) -> Line<'static> {
    let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    let mut spans = line.spans;
    if used < width {
        let modifiers = spans
            .last()
            .map(|s| s.style.add_modifier)
            .unwrap_or_default();
        spans.push(Span::styled(
            " ".repeat(width - used),
            bg_style(bg).add_modifier(modifiers),
        ));
    }
    Line::from(spans)
}

/// The inline block rendered under an annotation's last row.
fn annotation_rows(a: &Annotation, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let style = Style::default()
        .bg(theme.com_bg.unwrap_or(Color::Reset))
        .fg(theme.comment_fg);
    let block = |s: Style| {
        Style::default()
            .bg(theme.com_bg.unwrap_or(Color::Reset))
            .patch(s)
    };
    match &a.body {
        Body::Delete => vec![Line::from(Span::styled(
            truncate("    ✗ delete here", width),
            block(Style::default().fg(Color::Red)),
        ))],
        Body::Comment(text) | Body::Edit(text) => {
            let head = match a.kind() {
                Kind::Edit => "    ✎ edit: ",
                _ => "    ● ",
            };
            let mut out = vec![Line::from(Span::styled(
                truncate(
                    &format!("{head}{}", text.lines().next().unwrap_or("")),
                    width,
                ),
                style,
            ))];
            for extra in text.lines().skip(1) {
                out.push(Line::from(Span::styled(
                    truncate(&format!("    ┃ {extra}"), width),
                    style,
                )));
            }
            out
        }
    }
}

/// Runs the TUI until the user submits or quits. `Ok(true)` means submit.
///
/// The caller owns the terminal lifecycle; this only draws and reads events.
pub fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<bool> {
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if let Some(outcome) = app.outcome {
            return Ok(outcome);
        }
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(ev) => {
                    if app.handle_key(ev) {
                        return Ok(app.outcome.unwrap_or(false));
                    }
                }
                Event::Resize(..) => {}
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse;
    use crate::jj::Rev;

    fn target() -> Target {
        Target {
            revset: "@-".into(),
            base: Rev {
                commit_id: "000000000000".into(),
                description: String::new(),
            },
            head: Rev {
                commit_id: "42d29bfe9fd6".into(),
                description: "initial setup".into(),
            },
        }
    }

    const MOD: &str = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,4 @@
 one
-two
+TWO
+two and a half
 three
";

    const TWO_FILES: &str = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 one
-two
+TWO
diff --git a/b.txt b/b.txt
--- a/b.txt
+++ b/b.txt
@@ -5,2 +5,3 @@
 five
+six
 seven
";

    const MULTI_HUNK: &str = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 one
-two
+TWO
@@ -10,2 +10,2 @@
 ten
-ELEVEN
+eleven
";

    fn app_of(raw: &str) -> App {
        App::new(target(), parse(raw), None)
    }

    fn app() -> App {
        app_of(MOD)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_into_popup(a: &mut App, text: &str) {
        if let Some(p) = &mut a.popup {
            p.input.insert_text(text);
        }
    }

    #[test]
    fn starts_on_the_first_content_row() {
        let a = app();
        assert_eq!(a.focus, Focus::Diff);
        assert!(matches!(a.rows()[a.cursor()], Row::Line(_)));
    }

    #[test]
    fn movement_skips_hunk_separators() {
        let mut a = app();
        let first = a.cursor();
        for _ in 0..5 {
            a.move_cursor(1);
            assert!(matches!(a.rows()[a.cursor()], Row::Line(_)));
        }
        for _ in 0..5 {
            a.move_cursor(-1);
            assert!(matches!(a.rows()[a.cursor()], Row::Line(_)));
        }
        assert_eq!(a.cursor(), first, "clamped at the top");
    }

    #[test]
    fn hunk_keys_stay_within_the_file() {
        let mut a = app_of(TWO_FILES);
        assert_eq!(a.file, 0);
        let first = a.cursor();
        a.next_hunk();
        assert_eq!(a.file, 0, "file 0 has one hunk, so this wraps");
        assert_eq!(a.cursor(), first);
        a.prev_hunk();
        assert_eq!(a.file, 0);
        assert!(matches!(a.rows()[a.cursor()], Row::Line(_)));
    }

    #[test]
    fn hunk_keys_walk_hunks_then_wrap() {
        let mut a = app_of(MULTI_HUNK);
        let first = a.cursor();
        a.next_hunk();
        let second = a.cursor();
        assert!(second > first, "advanced to the second hunk");
        assert_eq!(a.file, 0);
        a.next_hunk();
        assert_eq!(
            a.cursor(),
            first,
            "past the last hunk it wraps to the first"
        );
        a.prev_hunk();
        assert_eq!(
            a.cursor(),
            second,
            "before the first hunk it wraps to the last"
        );
    }

    #[test]
    fn per_file_cursor_is_remembered() {
        let mut a = app_of(TWO_FILES);
        a.move_cursor(2);
        let remembered = a.cursor();
        a.next_file();
        assert_eq!(a.file, 1);
        assert_ne!(a.cursor(), remembered);
        a.prev_file();
        assert_eq!(a.cursor(), remembered);
    }

    #[test]
    fn selection_normalises_direction() {
        let mut a = app();
        a.set_cursor(5);
        a.vstart = Some(1);
        assert_eq!(a.selection(), Some((1, 5)));
    }

    #[test]
    fn comment_anchors_to_the_cursor_line() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Comment);
        type_into_popup(&mut a, "  needs a test  ");
        a.save_box();
        assert_eq!(a.annotations.len(), 1);
        let ann = &a.annotations[0];
        assert_eq!(ann.kind(), Kind::Comment);
        assert_eq!(ann.side, crate::model::Side::New);
        assert_eq!((ann.start, ann.end), (1, 1));
        assert_eq!(a_text(ann), "  needs a test", "whitespace trimmed");
    }

    #[test]
    fn empty_comment_is_discarded() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Comment);
        type_into_popup(&mut a, "   ");
        a.save_box();
        assert!(a.annotations.is_empty());
    }

    #[test]
    fn edit_prefilled_with_original_is_discarded() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Edit);
        // The popup starts with the line's own content, so saving is a no-op.
        a.save_box();
        assert!(a.annotations.is_empty());
    }

    #[test]
    fn edit_changed_content_is_kept() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Edit);
        if let Some(p) = &mut a.popup {
            p.input.move_end();
            p.input.insert_text("!");
        }
        a.save_box();
        assert_eq!(a.annotations.len(), 1);
        assert_eq!(a_text(&a.annotations[0]), "one!");
    }

    #[test]
    fn delete_on_a_removed_line_is_rejected() {
        let mut a = app();
        a.set_cursor(a.row_of_content(1));
        // Row 2 is the "-two" deletion: no new-side counterpart.
        assert_eq!(a.line_at_content(1).map(|l| l.kind), Some(LineKind::Del));
        a.mark_delete();
        assert!(a.annotations.is_empty());
        assert!(a.flash.is_some(), "the user gets an explanation");
    }

    #[test]
    fn delete_marks_and_clears() {
        let mut a = app();
        a.set_cursor(a.row_of_content(3));
        a.mark_delete();
        assert_eq!(a.annotations.len(), 1);
        assert_eq!(a.annotations[0].kind(), Kind::Delete);
        a.clear_at_cursor();
        assert!(a.annotations.is_empty());
    }

    #[test]
    fn reopening_a_comment_edits_it_in_place() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Comment);
        type_into_popup(&mut a, "first");
        a.save_box();
        let anchor_row = a.annotations[0].anchor_row;
        a.set_cursor(a.row_of_content(anchor_row));
        a.open_popup(Kind::Comment);
        type_into_popup(&mut a, " updated");
        a.save_box();
        assert_eq!(a.annotations.len(), 1, "in-place edit, not a duplicate");
        assert_eq!(a_text(&a.annotations[0]), "first updated");
    }

    #[test]
    fn comments_pane_ordering_and_goto() {
        let mut a = app_user_two_annotations();
        a.focus = Focus::Comments;
        assert_eq!(a.ordered_annotations().len(), 2);
        a.comment_cursor = 1;
        a.goto_selected_annotation();
        assert_eq!(a.focus, Focus::Diff);
        let expected = a.annotations[1].anchor_row;
        assert_eq!(a.content_of_row(a.cursor()), expected);
    }

    fn app_user_two_annotations() -> App {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Comment);
        type_into_popup(&mut a, "a");
        a.save_box();
        a.set_cursor(a.row_of_content(4));
        a.open_popup(Kind::Comment);
        type_into_popup(&mut a, "b");
        a.save_box();
        a
    }

    #[test]
    fn comments_pane_deletes_the_selected_annotation() {
        let mut a = app_user_two_annotations();
        a.focus = Focus::Comments;
        a.comment_cursor = 1;
        a.handle_key(key(KeyCode::Char('d')));
        assert_eq!(a.annotations.len(), 1);
        assert_eq!(a_text(&a.annotations[0]), "a");
    }

    #[test]
    fn per_file_badge_counts() {
        let a = app_user_two_annotations();
        assert_eq!(counts_for(&a.annotations, 0), (2, 0, 0));
    }

    #[test]
    fn file_pane_comment_is_file_scoped() {
        let mut a = app_of(TWO_FILES);
        a.focus = Focus::Files;
        a.handle_key(key(KeyCode::Char('c')));
        assert!(a.popup.is_some(), "the docked box opens");
        assert!(a.popup.as_ref().unwrap().file_scope);
        type_into_popup(&mut a, "this whole file needs tests");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.annotations.len(), 1);
        let ann = &a.annotations[0];
        assert!(ann.is_file_scope());
        assert_eq!(ann.label(), "whole file");
        assert_eq!((ann.start, ann.end), (0, 0));
        assert_eq!(a_text(ann), "this whole file needs tests");
        assert!(!ann.covers(0, &a.lines(0)[0]), "file scope covers no rows");
    }

    #[test]
    fn file_pane_comment_reopens_in_place() {
        let mut a = app_of(TWO_FILES);
        a.focus = Focus::Files;
        a.handle_key(key(KeyCode::Char('c')));
        type_into_popup(&mut a, "first");
        a.handle_key(key(KeyCode::Enter));
        a.handle_key(key(KeyCode::Char('c')));
        type_into_popup(&mut a, " updated");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.annotations.len(), 1, "edits in place, no duplicate");
        assert_eq!(a_text(&a.annotations[0]), "first updated");
    }

    #[test]
    fn file_pane_comment_follows_the_selected_file() {
        let mut a = app_of(TWO_FILES);
        a.focus = Focus::Files;
        a.next_file();
        assert_eq!(a.file, 1);
        a.handle_key(key(KeyCode::Char('c')));
        type_into_popup(&mut a, "on the second file");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.annotations[0].file, 1);
    }

    #[test]
    fn switching_files_does_not_retarget_an_open_box() {
        let mut a = app_of(TWO_FILES);
        a.set_cursor(a.row_of_content(0));
        a.handle_key(key(KeyCode::Char('c')));
        type_into_popup(&mut a, "for file zero");
        a.next_file();
        assert_eq!(a.file, 1, "the pane moved on");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.annotations.len(), 1);
        assert_eq!(
            a.annotations[0].file, 0,
            "anchored to the file it opened on"
        );
    }

    #[test]
    fn clear_file_annotations_drops_only_that_file() {
        let mut a = app_of(TWO_FILES);
        a.focus = Focus::Files;
        a.handle_key(key(KeyCode::Char('c')));
        type_into_popup(&mut a, "file zero");
        a.handle_key(key(KeyCode::Enter));
        a.next_file();
        a.handle_key(key(KeyCode::Char('c')));
        type_into_popup(&mut a, "file one");
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.annotations.len(), 2);

        a.handle_key(key(KeyCode::Char('d')));
        assert_eq!(a.annotations.len(), 1);
        assert_eq!(a.annotations[0].file, 0, "only file one was cleared");
    }

    #[test]
    fn input_panel_only_takes_space_while_open() {
        let mut a = app();
        assert_eq!(a.input_height(), 0);
        a.set_cursor(a.row_of_content(0));
        a.handle_key(key(KeyCode::Char('c')));
        assert!(a.input_height() >= 3);
        a.handle_key(key(KeyCode::Esc));
        assert_eq!(a.input_height(), 0, "the diff gets its rows back");
    }

    #[test]
    fn input_panel_growth_is_capped() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.handle_key(key(KeyCode::Char('c')));
        if let Some(p) = &mut a.popup {
            p.input.insert_text(&"x\n".repeat(40));
        }
        assert_eq!(a.input_height(), 10, "a long comment cannot eat the diff");
    }

    fn hint_keys(a: &App) -> Vec<&'static str> {
        a.context_hints().into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn hints_are_contextual_per_pane() {
        let mut a = app();
        let diff = hint_keys(&a);
        assert!(diff.contains(&"v"));

        a.focus = Focus::Files;
        let files = hint_keys(&a);
        assert!(files.contains(&"Enter"));
        assert!(!files.contains(&"v"), "selection is a diff-pane idea");

        a.focus = Focus::Comments;
        let comments = hint_keys(&a);
        assert!(comments.contains(&"Enter"));
        assert!(!comments.contains(&"] / ["), "hunk keys are diff-only");
    }

    #[test]
    fn hints_follow_the_selection_state() {
        let mut a = app();
        assert!(a.context_hints().iter().any(|(_, l)| *l == "select"));
        assert!(
            !a.context_hints()
                .iter()
                .any(|(_, l)| *l == "cancel selection")
        );

        a.handle_key(key(KeyCode::Char('v')));
        assert!(a.selection().is_some());
        assert!(
            a.context_hints()
                .iter()
                .any(|(_, l)| *l == "cancel selection"),
            "with a selection the labels describe range actions"
        );
        assert!(
            a.context_hints().iter().any(|(_, l)| *l == "comment range"),
            "c acts on the range once one exists"
        );
    }

    #[test]
    fn hints_switch_to_popup_and_help() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Comment);
        let popup = a.context_hints();
        assert!(popup.iter().any(|(k, _)| *k == "Ctrl+J"));
        assert!(
            !popup.iter().any(|(k, _)| *k == "s"),
            "submit is not a popup key"
        );

        a.popup = None;
        a.help = true;
        assert_eq!(a.context_hints(), vec![("any key", "close help")]);
    }

    #[test]
    fn tab_cycles_focus_both_ways() {
        let mut a = app();
        for expected in [Focus::Files, Focus::Comments, Focus::Diff] {
            a.handle_key(key(KeyCode::Tab));
            assert_eq!(a.focus, expected);
        }
        a.handle_key(key(KeyCode::BackTab));
        assert_eq!(a.focus, Focus::Comments);
    }

    #[test]
    fn popup_owns_esc_and_enter() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Comment);
        a.handle_key(key(KeyCode::Esc));
        assert!(a.popup.is_none());
        assert_eq!(a.outcome, None, "Esc closed the popup, not the app");

        a.open_popup(Kind::Comment);
        type_into_popup(&mut a, "kept");
        a.handle_key(key(KeyCode::Enter));
        assert!(a.popup.is_none());
        assert_eq!(a.annotations.len(), 1);
    }

    #[test]
    fn esc_clears_a_selection_before_quitting() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.handle_key(key(KeyCode::Char('v')));
        assert!(a.vstart.is_some());
        assert!(!a.handle_key(key(KeyCode::Esc)));
        assert!(a.vstart.is_none());
        assert_eq!(a.outcome, None);
    }

    #[test]
    fn submit_and_quit_set_the_outcome() {
        let mut a = app();
        assert!(a.handle_key(key(KeyCode::Char('s'))));
        assert_eq!(a.outcome, Some(true));

        let mut b = app();
        assert!(b.handle_key(key(KeyCode::Char('q'))));
        assert_eq!(b.outcome, Some(false));
    }

    #[test]
    fn ctrl_c_quits_from_any_pane() {
        let mut a = app();
        a.focus = Focus::Comments;
        let ev = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.handle_key(ev));
        assert_eq!(a.outcome, Some(false));
    }

    #[test]
    fn empty_diff_gets_an_empty_state() {
        let raw = "diff --git a/x b/x\n";
        let a = app_of(raw);
        assert!(a.empty_state.is_some());
    }

    fn row_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn gutters_show_both_sides_so_a_replacement_does_not_repeat() {
        // A modified file: two old lines replaced by two new ones, then a
        // single-line replacement further down.
        let raw = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -4,7 +4,7 @@
 line 4
-line 5
-line 6
+CHANGED five
+CHANGED six
 line 7
 line 8
 line 9
";
        let a = app_of(raw);
        let (lines, _) = a.diff_lines(60);
        let rendered: Vec<String> = lines.iter().map(row_text).collect();

        // Context rows carry the same number on both sides.
        assert!(
            rendered
                .iter()
                .any(|l| l.starts_with("     4    4   line 4")),
            "context row has both gutters:\n{}",
            rendered.join("\n")
        );
        // A deletion leaves the new gutter blank, and vice versa.
        assert!(
            rendered
                .iter()
                .any(|l| l.starts_with("     5      - line 5")),
            "deletion shows old number, blank new:\n{}",
            rendered.join("\n")
        );
        assert!(
            rendered
                .iter()
                .any(|l| l.starts_with("          5 + CHANGED five")),
            "addition shows new number, blank old:\n{}",
            rendered.join("\n")
        );
        // The old line 5 and the new line 5 are distinct rows, never one row
        // claiming two different numbers.
        assert_eq!(
            rendered
                .iter()
                .filter(|l| l.contains("5 + CHANGED five"))
                .count(),
            1
        );
    }

    #[test]
    fn added_file_leaves_the_old_gutter_empty() {
        let raw = "\
diff --git a/n.txt b/n.txt
new file mode 100644
--- /dev/null
+++ b/n.txt
@@ -0,0 +1,2 @@
+one
+two
";
        let a = app_of(raw);
        let (lines, _) = a.diff_lines(60);
        let rendered: Vec<String> = lines.iter().map(row_text).collect();
        assert!(
            rendered.iter().any(|l| l.starts_with("          1 + one")),
            "no old-side numbers on an added file:\n{}",
            rendered.join("\n")
        );
    }

    #[test]
    fn selection_marks_the_text_bold_and_keeps_one_background() {
        let mut a = app();
        a.handle_key(key(KeyCode::Char('v')));
        a.handle_key(key(KeyCode::Char('j')));
        assert!(a.selection().is_some());
        let (lines, _) = a.diff_lines(60);
        let (from, to) = a.selection().unwrap();
        let rows: Vec<(usize, &Line)> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.spans.iter().any(|s| s.style.bg == a.theme.sel_bg))
            .collect();
        assert!(!rows.is_empty(), "selected rows carry the selection bg");
        for (_, line) in rows {
            let bgs: Vec<_> = line.spans.iter().map(|s| s.style.bg).collect();
            let first = bgs.first().copied().unwrap();
            assert!(
                bgs.iter().all(|b| *b == first),
                "one background per selected row, got {bgs:?}"
            );
            assert!(
                line.spans
                    .iter()
                    .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
                "every span of a selected row is emphasised"
            );
        }
        let _ = (from, to);
    }

    #[test]
    fn unselected_rows_are_not_emphasised() {
        let a = app();
        let (lines, _) = a.diff_lines(60);
        assert!(
            !lines.iter().any(|l| l
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))),
            "bold is reserved for the selection"
        );
    }

    #[test]
    fn diff_lines_render_annotations_inline() {
        let mut a = app();
        a.set_cursor(a.row_of_content(0));
        a.open_popup(Kind::Comment);
        type_into_popup(&mut a, "hello\nthere");
        a.save_box();
        let (lines, cursor_line) = a.diff_lines(60);
        let rendered: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("● hello"),
            "comment header row:\n{rendered}"
        );
        assert!(rendered.contains("┃ there"), "continuation row");
        assert_eq!(cursor_line, 1, "cursor is on the first content row");
    }

    #[test]
    fn deleted_annotations_are_crossed_out() {
        let mut a = app();
        a.set_cursor(a.row_of_content(3));
        a.mark_delete();
        let (lines, _) = a.diff_lines(60);
        let crossed = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT))
        });
        assert!(crossed);
    }

    #[test]
    fn binary_files_show_a_placeholder() {
        let raw = "\
diff --git a/logo.png b/logo.png
Binary files a/logo.png and b/logo.png differ
";
        let files = parse(raw);
        let mut a = App::new(target(), files, None);
        a.set_cursor(a.row_of_content(0));
        assert!(a.is_binary());
        a.mark_delete();
        assert!(a.annotations.is_empty());
    }
}
