use std::time::Duration;

use color_eyre::Result;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    DefaultTerminal, Frame,
};

/// Top-level application state.
///
/// For now this is just a skeleton that can start, render, and quit. From step b
/// onward it will hold the results table (the Arrow `RecordBatch` and the
/// viewport offset).
#[derive(Debug, Default)]
pub struct App {
    /// Whether the main loop keeps running. Set to false to quit.
    running: bool,
}

impl App {
    pub fn new() -> Self {
        Self::default()
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

    /// Render a single frame, splitting the screen into header / body / footer.
    fn render(&self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1), // header (title)
            Constraint::Min(0),    // body (future results table)
            Constraint::Length(1), // footer (key hints)
        ])
        .areas(frame.area());

        // Header.
        frame.render_widget(
            Paragraph::new(" azuti ")
                .bold()
                .bg(Color::Blue)
                .fg(Color::White),
            header,
        );

        // Body: no data yet, so show a placeholder. In step c this is replaced
        // by the Arrow-backed, viewport-rendered results table.
        let placeholder = Paragraph::new(
            "\n  No data yet.\n\n  Roadmap:\n    b: show local data\n    c: fast table over synthetic data (Arrow + viewport rendering)\n    d: connect to Snowflake",
        )
        .block(Block::bordered().title(" Results "));
        frame.render_widget(placeholder, body);

        // Footer: key hints.
        let help = Line::from(vec![
            Span::styled(" q ", Style::new().fg(Color::Black).bg(Color::Gray)),
            Span::raw(" quit "),
        ]);
        frame.render_widget(Paragraph::new(help), footer);
    }

    /// Handle input events.
    ///
    /// Polling (rather than blocking on read) keeps the UI responsive and leaves
    /// room to interleave async work such as running a query later on.
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
            // Allow Ctrl-C to quit as well.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit(),
            _ => {}
        }
    }

    fn quit(&mut self) {
        self.running = false;
    }
}
