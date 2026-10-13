use std::path::PathBuf;
use std::time::Duration;

use color_eyre::Result;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    DefaultTerminal, Frame,
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use arrow::array::RecordBatch;
use tui_textarea::TextArea;

use crate::data::DataTable;
use crate::query::QueryEngine;

/// Rows beyond this are not scanned when measuring column widths (keeps load
/// cheap on large tables).
const WIDTH_SAMPLE_ROWS: usize = 200;
/// Per-column display width is clamped to this range.
const MIN_COL_WIDTH: u16 = 3;
const MAX_COL_WIDTH: u16 = 40;
/// How many lines Page Up / Page Down moves inside the detail overlay.
const DETAIL_PAGE_STEP: usize = 20;
/// Height (rows) of the SQL editor pane in interactive mode.
const EDITOR_HEIGHT: u16 = 8;

/// Which view currently has focus.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The results table.
    #[default]
    Table,
    /// A full-value overlay for the selected row.
    Detail,
}

/// Which pane has keyboard focus in interactive mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The results table.
    #[default]
    Results,
    /// The SQL editor.
    Editor,
}

/// Top-level application state.
///
/// The data stays in Arrow columns inside [`DataTable`]; the view formats only
/// the rows currently on screen (viewport rendering), so cost does not grow with
/// the total row count.
#[derive(Debug, Default)]
pub struct App {
    /// Whether the main loop keeps running. Set to false to quit.
    running: bool,
    /// The loaded data, if any.
    data: Option<DataTable>,
    /// Column headers.
    columns: Vec<String>,
    /// Per-column display widths, in terminal cells.
    col_widths: Vec<u16>,
    /// Total number of rows.
    nrows: usize,
    /// Absolute index of the selected row.
    selected: usize,
    /// Absolute index of the first visible row.
    offset: usize,
    /// Index of the first visible column (horizontal scroll).
    col_offset: usize,
    /// Number of body rows that fit on screen (updated each render).
    viewport_rows: usize,
    /// One-line status shown in the table title (load result or error).
    status: String,
    /// Current view (table or the row-detail overlay).
    mode: Mode,
    /// Vertical scroll offset within the detail overlay.
    detail_scroll: u16,
    /// SQL editor, present only in interactive mode.
    editor: Option<TextArea<'static>>,
    /// Background query worker, present only in interactive mode.
    engine: Option<QueryEngine>,
    /// Which pane has focus (interactive mode).
    focus: Focus,
}

impl App {
    /// Build the app, loading `path` if one was given. A load error is captured
    /// into the status line rather than crashing the UI.
    pub fn new(path: Option<PathBuf>) -> Self {
        match path {
            None => App::empty("No file given. Pass a .csv or .parquet path, or use --demo N."),
            Some(path) => match DataTable::load(&path) {
                Ok(data) => {
                    let status = format!(
                        "{}  —  {} rows × {} cols",
                        path.display(),
                        data.num_rows(),
                        data.num_cols()
                    );
                    App::loaded(data, status)
                }
                // `{:#}` prints the whole eyre error chain on one line.
                Err(err) => App::empty(&format!("Error loading {}: {:#}", path.display(), err)),
            },
        }
    }

    /// Build the app on top of a synthetic table (for stress-testing).
    pub fn from_demo(nrows: usize) -> Self {
        let data = DataTable::demo(nrows);
        let status = format!(
            "demo  —  {} rows × {} cols",
            data.num_rows(),
            data.num_cols()
        );
        App::loaded(data, status)
    }

    /// Build the app on top of an already-loaded table (e.g. a query result).
    pub fn from_table(data: DataTable, status: String) -> Self {
        App::loaded(data, status)
    }

    fn empty(status: &str) -> Self {
        App {
            status: status.to_string(),
            ..Default::default()
        }
    }

    fn loaded(data: DataTable, status: String) -> Self {
        let mut app = App {
            status,
            ..Default::default()
        };
        app.apply_table(data);
        app
    }

    /// Build the app in interactive mode: a SQL editor backed by a query engine.
    pub fn interactive(engine: QueryEngine) -> Self {
        let mut editor = TextArea::default();
        editor.set_placeholder_text("SELECT …   (Ctrl+Enter or Ctrl+R to run)");
        App {
            editor: Some(editor),
            engine: Some(engine),
            focus: Focus::Editor,
            status: "Write a query and press Ctrl+Enter (or Ctrl+R)".to_string(),
            ..Default::default()
        }
    }

    /// Replace the current table with `data`, resetting scroll and selection.
    fn apply_table(&mut self, data: DataTable) {
        self.columns = data.column_names();
        self.nrows = data.num_rows();
        self.col_widths = data
            .column_display_widths(WIDTH_SAMPLE_ROWS)
            .map(|ws| ws.into_iter().map(clamp_width).collect())
            .unwrap_or_else(|_| self.columns.iter().map(|_| MIN_COL_WIDTH).collect());
        self.selected = 0;
        self.offset = 0;
        self.col_offset = 0;
        self.data = Some(data);
    }

    fn has_rows(&self) -> bool {
        self.data.is_some() && self.nrows > 0
    }

    /// Main loop: draw, handle input, then apply any finished query.
    pub fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        self.running = true;
        while self.running {
            terminal.draw(|frame| self.render(frame))?;
            self.handle_events()?;
            self.poll_query();
        }
        Ok(())
    }

    /// Check the query engine for a finished result and apply it.
    fn poll_query(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        if let Some(outcome) = engine.poll() {
            match outcome {
                Ok(batches) => self.load_result(batches),
                Err(err) => self.status = format!("Query error: {err}"),
            }
        }
    }

    fn load_result(&mut self, batches: Vec<RecordBatch>) {
        match DataTable::from_batches(batches) {
            Ok(data) => {
                let ncols = data.num_cols();
                self.apply_table(data);
                self.status = format!("{} rows × {} cols", self.nrows, ncols);
                self.mode = Mode::Table;
                self.focus = Focus::Results;
            }
            Err(_) => {
                self.data = None;
                self.nrows = 0;
                self.columns.clear();
                self.col_widths.clear();
                self.status = "0 rows".to_string();
            }
        }
    }

    /// Render a single frame, dispatching on whether the editor is present.
    fn render(&mut self, frame: &mut Frame) {
        if self.editor.is_some() {
            self.render_interactive(frame);
        } else {
            self.render_static(frame);
        }
    }

    /// Static viewer: title bar / results / footer.
    fn render_static(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        frame.render_widget(
            Paragraph::new(" azuti ")
                .bold()
                .bg(Color::Blue)
                .fg(Color::White),
            header,
        );

        self.render_body(frame, body);
        self.render_detail_overlay(frame);
        frame.render_widget(Paragraph::new(self.footer_help()), footer);
    }

    /// Interactive mode: SQL editor / results / footer.
    fn render_interactive(&mut self, frame: &mut Frame) {
        let [top, body, footer] = Layout::vertical([
            Constraint::Length(EDITOR_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let focused = self.focus == Focus::Editor;
        if let Some(editor) = &mut self.editor {
            let border = if focused { Color::Blue } else { Color::DarkGray };
            editor.set_block(
                Block::bordered()
                    .title(" SQL — Ctrl+Enter / Ctrl+R to run ")
                    .border_style(Style::new().fg(border)),
            );
            frame.render_widget(&*editor, top);
        }

        self.render_body(frame, body);
        self.render_detail_overlay(frame);
        frame.render_widget(Paragraph::new(self.footer_help()), footer);
    }

    /// Render the results table, or the status line when there is no data.
    fn render_body(&mut self, frame: &mut Frame, area: Rect) {
        if self.has_rows() {
            self.render_table(frame, area);
        } else {
            let block = Block::bordered().title(" Results ");
            frame.render_widget(Paragraph::new(self.status.clone()).block(block), area);
        }
    }

    fn render_detail_overlay(&self, frame: &mut Frame) {
        if self.mode == Mode::Detail {
            let full = frame.area();
            self.render_detail(frame, full);
        }
    }

    fn footer_help(&self) -> Line<'static> {
        if self.mode == Mode::Detail {
            return Line::from(vec![
                key(" ↑/↓ j/k "),
                Span::raw(" scroll  "),
                key(" g/G "),
                Span::raw(" top/bottom  "),
                key(" Esc "),
                Span::raw(" close "),
            ]);
        }
        if self.editor.is_some() && self.focus == Focus::Editor {
            return Line::from(vec![
                key(" Ctrl+Enter "),
                Span::raw(" / "),
                key(" Ctrl+R "),
                Span::raw(" run  "),
                key(" Tab "),
                Span::raw(" results  "),
                key(" Ctrl-C "),
                Span::raw(" quit "),
            ]);
        }

        let mut spans = vec![
            key(" ↑/↓ j/k "),
            Span::raw(" move  "),
            key(" ←/→ h/l "),
            Span::raw(" cols  "),
            key(" g/G "),
            Span::raw(" top/bottom  "),
            key(" Enter "),
            Span::raw(" details  "),
        ];
        if self.editor.is_some() {
            spans.push(key(" Tab "));
            spans.push(Span::raw(" editor  "));
        }
        spans.push(key(" q "));
        spans.push(Span::raw(" quit "));
        Line::from(spans)
    }

    /// Render only the rows inside the current viewport. Cells are formatted from
    /// the Arrow columns on demand, so work is bounded by what is on screen.
    fn render_table(&mut self, frame: &mut Frame, area: Rect) {
        // Inner height minus the two borders and the header row.
        let visible = area.height.saturating_sub(3) as usize;
        self.viewport_rows = visible.max(1);
        self.clamp_offset();

        // Format just the visible window once; reuse it for sizing and rendering.
        let end = (self.offset + self.viewport_rows).min(self.nrows);
        let visible: Vec<Vec<String>> = {
            let data = self.data.as_ref().expect("has_rows checked before render_table");
            match data.formatters() {
                Ok(formatters) => (self.offset..end)
                    .map(|r| formatters.iter().map(|f| f.value(r).to_string()).collect())
                    .collect(),
                Err(err) => {
                    let block = Block::bordered().title(" Results ");
                    frame.render_widget(
                        Paragraph::new(format!("Render error: {err:#}")).block(block),
                        area,
                    );
                    return;
                }
            }
        };

        // Grow column widths so values scrolled into view always fit. Widths only
        // ever grow (capped at MAX_COL_WIDTH), so they do not jitter while
        // scrolling and later, wider values are not clipped by an early estimate.
        for row in &visible {
            for (c, cell) in row.iter().enumerate() {
                let w = clamp_width(cell.width());
                if w > self.col_widths[c] {
                    self.col_widths[c] = w;
                }
            }
        }

        // Decide which columns fit, starting from the horizontal offset.
        let inner_width = area.width.saturating_sub(2);
        self.clamp_col_offset(inner_width);
        let cols = self.visible_cols(inner_width);

        let rows: Vec<Row> = visible
            .iter()
            .map(|row| {
                Row::new(
                    row[cols.clone()]
                        .iter()
                        .zip(&self.col_widths[cols.clone()])
                        .map(|(cell, w)| Cell::from(truncate_display(cell, *w)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        let header_row = Row::new(
            self.columns[cols.clone()]
                .iter()
                .zip(&self.col_widths[cols.clone()])
                .map(|(c, w)| Cell::from(truncate_display(c, *w)))
                .collect::<Vec<_>>(),
        )
        // Give the header a filled bar so it reads clearly apart from the data
        // rows (and from the reversed selection highlight).
        .style(Style::new().bold().fg(Color::White).bg(Color::Blue));

        let constraints: Vec<Constraint> = self.col_widths[cols.clone()]
            .iter()
            .map(|w| Constraint::Length(*w))
            .collect();

        // `‹` / `›` show that more columns exist off the left / right edge.
        let title = format!(
            " {left}{status}  —  row {row}/{nrows}  col {c0}-{c1}/{ncols}{right} ",
            left = if cols.start > 0 { "‹ " } else { "" },
            right = if cols.end < self.columns.len() { " ›" } else { "" },
            status = self.status,
            row = self.selected + 1,
            nrows = self.nrows,
            c0 = cols.start + 1,
            c1 = cols.end,
            ncols = self.columns.len(),
        );
        let table = Table::new(rows, constraints)
            .header(header_row)
            .row_highlight_style(Style::new().reversed())
            .highlight_symbol("> ")
            .block(Block::bordered().title(title));

        // The table is handed only the visible slice, so the highlighted row is
        // addressed relative to the current offset.
        let mut view_state = TableState::default();
        view_state.select(Some(self.selected - self.offset));
        frame.render_stateful_widget(table, area, &mut view_state);
    }

    /// Scroll the viewport so the selected row stays visible.
    fn clamp_offset(&mut self) {
        let visible = self.viewport_rows.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }
        let max_offset = self.nrows.saturating_sub(visible);
        self.offset = self.offset.min(max_offset);
    }

    /// Reserved cells for the row-selection symbol (`"> "`) on the left.
    const HIGHLIGHT_WIDTH: u16 = 2;

    /// Clamp the horizontal offset so scrolling right stops once the last column
    /// is fully visible and the width is filled.
    fn clamp_col_offset(&mut self, inner_width: u16) {
        let ncols = self.columns.len();
        if ncols == 0 {
            self.col_offset = 0;
            return;
        }
        let avail = inner_width.saturating_sub(Self::HIGHLIGHT_WIDTH);
        let mut used = 0u16;
        let mut max_offset = ncols - 1;
        for c in (0..ncols).rev() {
            let add = self.col_widths[c] + if c == ncols - 1 { 0 } else { 1 };
            if c != ncols - 1 && used + add > avail {
                break;
            }
            used = used.saturating_add(add);
            max_offset = c;
        }
        self.col_offset = self.col_offset.min(max_offset);
    }

    /// The range of columns that fit on screen from the current offset. At least
    /// one column is always shown, even if it is wider than the screen.
    fn visible_cols(&self, inner_width: u16) -> std::ops::Range<usize> {
        let ncols = self.columns.len();
        if ncols == 0 {
            return 0..0;
        }
        let avail = inner_width.saturating_sub(Self::HIGHLIGHT_WIDTH);
        let start = self.col_offset.min(ncols - 1);
        let mut used = 0u16;
        let mut end = start;
        for c in start..ncols {
            let add = self.col_widths[c] + if c == start { 0 } else { 1 };
            if c != start && used + add > avail {
                break;
            }
            used = used.saturating_add(add);
            end = c + 1;
        }
        start..end
    }

    /// Move the horizontal column offset by `delta`, clamped to the columns.
    fn move_col(&mut self, delta: isize) {
        let ncols = self.columns.len();
        if !self.has_rows() || ncols == 0 {
            return;
        }
        let last = (ncols - 1) as isize;
        self.col_offset = (self.col_offset as isize + delta).clamp(0, last) as usize;
    }

    fn handle_events(&mut self) -> Result<()> {
        // Poll briefly so a finished query is picked up promptly even while idle.
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    self.on_key(key);
                }
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Ctrl-C always quits, in any mode.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit();
            return;
        }
        if self.mode == Mode::Detail {
            self.on_key_detail(key);
            return;
        }
        if self.editor.is_some() && self.focus == Focus::Editor {
            self.on_key_editor(key);
        } else {
            self.on_key_table(key);
        }
    }

    fn on_key_editor(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Ctrl+Enter runs on terminals that can report it (needs the keyboard
            // enhancement protocol); Ctrl+R runs on every terminal.
            KeyCode::Enter if ctrl => self.run_query(),
            KeyCode::Char('r') if ctrl => self.run_query(),
            KeyCode::Tab | KeyCode::Esc => self.focus = Focus::Results,
            _ => {
                if let Some(editor) = &mut self.editor {
                    editor.input(key);
                }
            }
        }
    }

    /// Submit the editor's SQL to the query engine.
    fn run_query(&mut self) {
        if self.engine.is_none() {
            return;
        }
        let sql = match &self.editor {
            Some(editor) => editor.lines().join("\n"),
            None => return,
        };
        if sql.trim().is_empty() {
            self.status = "Write a query first".to_string();
            return;
        }
        if let Some(engine) = &self.engine {
            engine.submit(sql);
        }
        self.status = "running…".to_string();
    }

    fn on_key_table(&mut self, key: KeyEvent) {
        let page = self.viewport_rows.max(1) as isize;
        match key.code {
            KeyCode::Tab if self.editor.is_some() => self.focus = Focus::Editor,
            KeyCode::Esc if self.editor.is_some() => self.focus = Focus::Editor,
            KeyCode::Char('q') | KeyCode::Esc => self.quit(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Left | KeyCode::Char('h') => self.move_col(-1),
            KeyCode::Right | KeyCode::Char('l') => self.move_col(1),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => self.select(self.nrows.saturating_sub(1)),
            KeyCode::Enter => self.open_detail(),
            _ => {}
        }
    }

    fn on_key_detail(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.close_detail(),
            KeyCode::Down | KeyCode::Char('j') => self.detail_scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.detail_scroll_by(-1),
            KeyCode::PageDown => self.detail_scroll_by(DETAIL_PAGE_STEP as isize),
            KeyCode::PageUp => self.detail_scroll_by(-(DETAIL_PAGE_STEP as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.detail_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.detail_scroll = self.detail_max_scroll(),
            _ => {}
        }
    }

    /// Move the selection by `delta` rows, clamped to the table bounds.
    fn move_selection(&mut self, delta: isize) {
        if !self.has_rows() {
            return;
        }
        let last = (self.nrows - 1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    fn select(&mut self, index: usize) {
        if !self.has_rows() {
            return;
        }
        self.selected = index.min(self.nrows - 1);
    }

    /// Open the detail overlay for the selected row.
    fn open_detail(&mut self) {
        if !self.has_rows() {
            return;
        }
        self.mode = Mode::Detail;
        self.detail_scroll = 0;
    }

    fn close_detail(&mut self) {
        self.mode = Mode::Table;
    }

    fn detail_scroll_by(&mut self, delta: isize) {
        let max = self.detail_max_scroll() as isize;
        let next = (self.detail_scroll as isize + delta).clamp(0, max);
        self.detail_scroll = next as u16;
    }

    /// Rough upper bound on how far the detail overlay can scroll, estimated
    /// from field widths (exact wrapping depends on the popup size at render).
    fn detail_max_scroll(&self) -> u16 {
        const ASSUMED_WIDTH: usize = 60;
        let Some(data) = self.data.as_ref() else {
            return 0;
        };
        let row = data.format_row(self.selected).unwrap_or_default();
        let lines: usize = self
            .columns
            .iter()
            .zip(&row)
            .map(|(name, value)| (name.width() + 2 + value.width()) / ASSUMED_WIDTH + 1)
            .sum();
        lines.saturating_sub(1) as u16
    }

    /// Render the selected row's full, untruncated values as a centered overlay.
    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data.as_ref() else {
            return;
        };
        let row = data.format_row(self.selected).unwrap_or_default();
        if row.is_empty() {
            return;
        }

        let popup = centered_rect(area, 80, 80);
        frame.render_widget(Clear, popup);

        let lines: Vec<Line> = self
            .columns
            .iter()
            .zip(&row)
            .map(|(name, value)| {
                Line::from(vec![
                    Span::styled(format!("{name}: "), Style::new().fg(Color::Cyan).bold()),
                    Span::raw(value.clone()),
                ])
            })
            .collect();

        let title = format!(" Row {} / {} — Esc to close ", self.selected + 1, self.nrows);
        let detail = Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.detail_scroll, 0));
        frame.render_widget(detail, popup);
    }

    fn quit(&mut self) {
        self.running = false;
    }
}

/// A small inverted key-cap span for the footer hints.
fn key(label: &str) -> Span<'static> {
    Span::styled(
        label.to_string(),
        Style::new().fg(Color::Black).bg(Color::Gray),
    )
}

/// Clamp a measured display width into the allowed column-width range.
fn clamp_width(w: usize) -> u16 {
    w.clamp(MIN_COL_WIDTH as usize, MAX_COL_WIDTH as usize) as u16
}

/// A rectangle centered within `area`, sized to the given percentages of it.
fn centered_rect(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let width = area.width * pct_x / 100;
    let height = area.height * pct_y / 100;
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// Truncate `s` to at most `max` display cells, appending `…` when it is cut.
/// Width-aware so a full-width character is never split across the boundary.
fn truncate_display(s: &str, max: u16) -> String {
    let max = max as usize;
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Reserve one cell for the ellipsis.
    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    /// Render a loaded file to an in-memory terminal and return the frame as text.
    fn render(path: &str, w: u16, h: u16) -> String {
        let full = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
        let mut app = App::new(Some(full));
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn long_cells_are_truncated_with_ellipsis() {
        let out = render("testdata/long_text.csv", 100, 12);
        assert!(
            out.contains('…'),
            "expected an ellipsis on clipped cells:\n{out}"
        );
    }

    #[test]
    fn renders_without_panic() {
        // Full-width text and a wide (40-column) table must both render.
        assert!(render("testdata/long_text.csv", 100, 12).contains("azuti"));
        assert!(render("testdata/wide.csv", 100, 12).contains("azuti"));
    }

    #[test]
    fn truncate_display_respects_width_and_fullwidth() {
        assert_eq!(truncate_display("hello", 10), "hello");
        assert_eq!(truncate_display("hello world", 8), "hello w…");
        // Each CJK char is 2 cells: width 5 fits two chars (4) + ellipsis (1).
        assert_eq!(truncate_display("あいうえお", 5), "あい…");
    }

    #[test]
    fn detail_overlay_shows_untruncated_value() {
        let full = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/long_text.csv");
        let mut app = App::new(Some(full));
        app.select(2); // the row with the very long note
        app.open_detail();

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let out = format!("{}", terminal.backend());

        // "description" sits past the 40-cell table cap, so it only appears when
        // the full value is shown in the detail overlay.
        assert!(
            out.contains("description"),
            "detail overlay should show the full value:\n{out}"
        );
    }

    #[test]
    fn viewport_renders_only_visible_rows() {
        // A 1000-row synthetic table: only the on-screen slice is rendered.
        let mut app = App::from_demo(1000);
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();

        terminal.draw(|f| app.render(f)).unwrap();
        let top = format!("{}", terminal.backend());
        assert!(top.contains("item-0"), "top of the table should render:\n{top}");
        assert!(
            !top.contains("item-999"),
            "the last row must not render while scrolled to the top"
        );

        app.select(999);
        terminal.draw(|f| app.render(f)).unwrap();
        let bottom = format!("{}", terminal.backend());
        assert!(
            bottom.contains("item-999"),
            "scrolling to the end should reveal the last row:\n{bottom}"
        );
    }

    #[test]
    fn columns_grow_to_fit_values_scrolled_into_view() {
        // Columns are sized from the first rows, where ids are narrow. A much
        // wider id deep in the data must still be shown in full once scrolled to.
        let mut app = App::from_demo(1_000_000);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();

        terminal.draw(|f| app.render(f)).unwrap(); // size from the first page
        app.select(999_999);
        terminal.draw(|f| app.render(f)).unwrap();
        let out = format!("{}", terminal.backend());

        assert!(
            out.contains("item-999999"),
            "a wide value scrolled into view must not be clipped:\n{out}"
        );
    }

    #[test]
    fn header_row_is_visually_distinct() {
        let full = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/sample.csv");
        let mut app = App::new(Some(full));
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();

        let buf = terminal.backend().buffer();
        // y0 is the title bar, y1 the top border, y2 the header row, y3 data.
        assert!(
            (1u16..59).all(|x| buf[(x, 2)].style().bg == Some(Color::Blue)),
            "header row should be a filled bar"
        );
        assert!(
            (1u16..59).all(|x| buf[(x, 3)].style().bg != Some(Color::Blue)),
            "data rows should not share the header's fill"
        );
    }

    #[test]
    fn interactive_layout_shows_editor_and_footer() {
        // Build an interactive-looking app without a live engine.
        let mut app = App {
            editor: Some(TextArea::default()),
            focus: Focus::Editor,
            status: "Write a query".to_string(),
            ..Default::default()
        };

        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let out = format!("{}", terminal.backend());

        assert!(out.contains("SQL"), "editor pane should be labelled:\n{out}");
        assert!(out.contains("Ctrl+R"), "footer should show the run hint:\n{out}");
    }

    #[test]
    fn horizontal_scroll_reveals_right_columns() {
        // wide.csv has 40 columns; only the left ones fit at first.
        let full = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/wide.csv");
        let mut app = App::new(Some(full));
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();

        terminal.draw(|f| app.render(f)).unwrap();
        let left = format!("{}", terminal.backend());
        assert!(left.contains("col01"), "left columns should be visible:\n{left}");
        assert!(
            !left.contains("col40"),
            "the last column must be off-screen before scrolling:\n{left}"
        );

        app.move_col(100); // scroll fully right (render clamps to the end)
        terminal.draw(|f| app.render(f)).unwrap();
        let right = format!("{}", terminal.backend());
        assert!(
            right.contains("col40"),
            "scrolling right should reveal the last column:\n{right}"
        );
    }
}
