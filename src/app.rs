use std::path::PathBuf;
use std::time::Duration;

use color_eyre::Result;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Cell, Paragraph, Row, Table, TableState},
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

        let help = Line::from(vec![
            key(" ↑/↓ j/k "),
            Span::raw(" move  "),
            key(" g/G "),
            Span::raw(" top/bottom  "),
            key(" q "),
            Span::raw(" quit "),
        ]);
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(PAGE_STEP as isize),
            KeyCode::PageUp => self.move_selection(-(PAGE_STEP as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => self.select(self.rows.len().saturating_sub(1)),
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
}
