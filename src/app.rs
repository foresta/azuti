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

use crate::data::DataTable;

/// Rows beyond this are not scanned when measuring column widths (keeps load
/// cheap on large files; step c revisits sizing).
const WIDTH_SAMPLE_ROWS: usize = 200;
/// Per-column display width is clamped to this range.
const MIN_COL_WIDTH: u16 = 3;
const MAX_COL_WIDTH: u16 = 40;
/// How many rows Page Up / Page Down moves the selection.
const PAGE_STEP: usize = 20;

/// Which view currently has focus.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The results table.
    #[default]
    Table,
    /// A full-value overlay for the selected row.
    Detail,
}

/// Top-level application state.
#[derive(Debug, Default)]
pub struct App {
    /// Whether the main loop keeps running. Set to false to quit.
    running: bool,
    /// Column headers.
    columns: Vec<String>,
    /// Rows, each already formatted to display strings (step b baseline).
    rows: Vec<Vec<String>>,
    /// Precomputed per-column display widths, in terminal cells.
    col_widths: Vec<u16>,
    /// Table selection / scroll position.
    table_state: TableState,
    /// One-line status shown as the table title (load result or error).
    status: String,
    /// Current view (table or the row-detail overlay).
    mode: Mode,
    /// Vertical scroll offset within the detail overlay.
    detail_scroll: u16,
}

impl App {
    /// Build the app, loading `path` if one was given. A load error is captured
    /// into the status line rather than crashing the UI.
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut app = App::default();
        match path {
            None => {
                app.status = "No file given. Pass a .csv or .parquet path.".to_string();
            }
            Some(path) => match DataTable::load(&path).and_then(|t| Ok((t.column_names(), t.num_rows(), t.num_cols(), t.to_string_rows()?))) {
                Ok((columns, nrows, ncols, rows)) => {
                    app.col_widths = column_widths(&columns, &rows);
                    app.columns = columns;
                    app.rows = rows;
                    if !app.rows.is_empty() {
                        app.table_state.select(Some(0));
                    }
                    app.status = format!("{}  —  {} rows × {} cols", path.display(), nrows, ncols);
                }
                Err(err) => {
                    // `{:#}` prints the whole eyre error chain on one line.
                    app.status = format!("Error loading {}: {:#}", path.display(), err);
                }
            },
        }
        app
    }

    /// Main loop: draw, then handle input, repeated while `running` is set.
    pub fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        self.running = true;
        while self.running {
            terminal.draw(|frame| self.render(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    /// Render a single frame: header / body / footer.
    fn render(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1), // header (title)
            Constraint::Min(0),    // body (results table or message)
            Constraint::Length(1), // footer (key hints)
        ])
        .areas(frame.area());

        frame.render_widget(
            Paragraph::new(" azuti ")
                .bold()
                .bg(Color::Blue)
                .fg(Color::White),
            header,
        );

        if self.rows.is_empty() {
            // No data (nothing loaded, empty file, or an error): show the status.
            let block = Block::bordered().title(" Results ");
            frame.render_widget(Paragraph::new(self.status.clone()).block(block), body);
        } else {
            let header_row = Row::new(
                self.columns
                    .iter()
                    .zip(&self.col_widths)
                    .map(|(c, w)| Cell::from(truncate_display(c, *w))),
            )
            .style(Style::new().bold());

            let body_rows: Vec<Row> = self
                .rows
                .iter()
                .map(|r| {
                    Row::new(
                        r.iter()
                            .zip(&self.col_widths)
                            .map(|(c, w)| Cell::from(truncate_display(c, *w))),
                    )
                })
                .collect();

            let constraints: Vec<Constraint> =
                self.col_widths.iter().map(|w| Constraint::Length(*w)).collect();
            let table = Table::new(body_rows, constraints)
                .header(header_row)
                .row_highlight_style(Style::new().reversed())
                .highlight_symbol("> ")
                .block(Block::bordered().title(format!(" {} ", self.status)));

            frame.render_stateful_widget(table, body, &mut self.table_state);
        }

        // The detail overlay (if open) sits on top of the table.
        if self.mode == Mode::Detail {
            let full = frame.area();
            self.render_detail(frame, full);
        }

        let help = match self.mode {
            Mode::Table => Line::from(vec![
                key(" ↑/↓ j/k "),
                Span::raw(" move  "),
                key(" g/G "),
                Span::raw(" top/bottom  "),
                key(" Enter "),
                Span::raw(" details  "),
                key(" q "),
                Span::raw(" quit "),
            ]),
            Mode::Detail => Line::from(vec![
                key(" ↑/↓ j/k "),
                Span::raw(" scroll  "),
                key(" g/G "),
                Span::raw(" top/bottom  "),
                key(" Esc "),
                Span::raw(" close "),
            ]),
        };
        frame.render_widget(Paragraph::new(help), footer);
    }

    fn handle_events(&mut self) -> Result<()> {
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
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
        match self.mode {
            Mode::Table => self.on_key_table(key),
            Mode::Detail => self.on_key_detail(key),
        }
    }

    fn on_key_table(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(PAGE_STEP as isize),
            KeyCode::PageUp => self.move_selection(-(PAGE_STEP as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => self.select(self.rows.len().saturating_sub(1)),
            KeyCode::Enter => self.open_detail(),
            _ => {}
        }
    }

    fn on_key_detail(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.close_detail(),
            KeyCode::Down | KeyCode::Char('j') => self.detail_scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.detail_scroll_by(-1),
            KeyCode::PageDown => self.detail_scroll_by(PAGE_STEP as isize),
            KeyCode::PageUp => self.detail_scroll_by(-(PAGE_STEP as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.detail_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.detail_scroll = self.detail_max_scroll(),
            _ => {}
        }
    }

    /// Move the selection by `delta` rows, clamped to the table bounds.
    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.table_state.selected().unwrap_or(0) as isize;
        let last = (self.rows.len() - 1) as isize;
        let next = (current + delta).clamp(0, last) as usize;
        self.table_state.select(Some(next));
    }

    fn select(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.table_state.select(Some(index.min(self.rows.len() - 1)));
    }

    /// Open the detail overlay for the selected row.
    fn open_detail(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.table_state.selected().is_none() {
            self.table_state.select(Some(0));
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
        let Some(row) = self.table_state.selected().and_then(|i| self.rows.get(i)) else {
            return 0;
        };
        let lines: usize = self
            .columns
            .iter()
            .zip(row)
            .map(|(name, value)| (name.width() + 2 + value.width()) / ASSUMED_WIDTH + 1)
            .sum();
        lines.saturating_sub(1) as u16
    }

    /// Render the selected row's full, untruncated values as a centered overlay.
    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        let Some((i, row)) = self
            .table_state
            .selected()
            .and_then(|i| self.rows.get(i).map(|r| (i, r)))
        else {
            return;
        };

        let popup = centered_rect(area, 80, 80);
        frame.render_widget(Clear, popup);

        let lines: Vec<Line> = self
            .columns
            .iter()
            .zip(row)
            .map(|(name, value)| {
                Line::from(vec![
                    Span::styled(format!("{name}: "), Style::new().fg(Color::Cyan).bold()),
                    Span::raw(value.clone()),
                ])
            })
            .collect();

        let title = format!(" Row {} / {} — Esc to close ", i + 1, self.rows.len());
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
fn key(label: &str) -> Span<'_> {
    Span::styled(
        label.to_string(),
        Style::new().fg(Color::Black).bg(Color::Gray),
    )
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

/// Measure a display width (terminal cells) per column from the header plus a
/// sample of rows. Uses Unicode display width so full-width characters count
/// as 2, matching how the terminal renders them.
fn column_widths(columns: &[String], rows: &[Vec<String>]) -> Vec<u16> {
    columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mut width = name.width();
            for row in rows.iter().take(WIDTH_SAMPLE_ROWS) {
                if let Some(cell) = row.get(i) {
                    width = width.max(cell.width());
                }
            }
            (width as u16).clamp(MIN_COL_WIDTH, MAX_COL_WIDTH)
        })
        .collect()
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
}
