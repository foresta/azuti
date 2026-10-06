mod app;
mod data;

use app::App;
use clap::Parser;
use color_eyre::Result;

/// azuti — a fast terminal (TUI) data viewer.
///
/// The name comes from kyudo (Japanese archery): the "azuchi" is the target
/// mound where arrows fly, land, and line up in view — a fitting metaphor for a
/// data viewer. Spelled `azuti` in Kunrei-shiki romanization.
#[derive(Debug, Parser)]
#[command(name = "azuti", version, about)]
struct Cli {
    /// Local data file to open (CSV / Parquet, etc.).
    path: Option<std::path::PathBuf>,

    /// Generate N synthetic rows instead of loading a file (for stress testing).
    #[arg(long, value_name = "ROWS")]
    demo: Option<usize>,
}

fn main() -> Result<()> {
    // Pretty-print panics and errors.
    // `ratatui::init()` below layers its own terminal-restoring panic hook on
    // top, so install color_eyre first, then init ratatui.
    color_eyre::install()?;

    let cli = Cli::parse();
    let app = match cli.demo {
        Some(rows) => App::from_demo(rows),
        None => App::new(cli.path),
    };

    // Switch the terminal into raw mode + alternate screen, and always restore
    // it on exit.
    let terminal = ratatui::init();
    let result = app.run(terminal);
    ratatui::restore();
    result
}
