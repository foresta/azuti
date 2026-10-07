mod app;
mod data;
mod snowflake;

use app::App;
use clap::Parser;
use color_eyre::eyre::{bail, Result};

use crate::data::DataTable;
use crate::snowflake::SnowflakeParams;

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

    /// Run this SQL against Snowflake and show the result.
    #[arg(long, value_name = "SQL")]
    sql: Option<String>,

    /// Snowflake account identifier (e.g. ab12345.ap-northeast-1.aws).
    #[arg(long)]
    account: Option<String>,
    /// Snowflake username.
    #[arg(long)]
    user: Option<String>,
    /// Path to the PEM private key for key-pair (JWT) auth.
    #[arg(long, value_name = "PATH")]
    private_key: Option<std::path::PathBuf>,
    /// Warehouse to use for the query.
    #[arg(long)]
    warehouse: Option<String>,
    /// Default database.
    #[arg(long)]
    database: Option<String>,
    /// Default schema.
    #[arg(long)]
    schema: Option<String>,
    /// Role to assume (use a read-only role for viewing).
    #[arg(long)]
    role: Option<String>,
}

impl Cli {
    /// Assemble Snowflake connection parameters, erroring if a required one is
    /// missing.
    fn snowflake_params(&self) -> Result<SnowflakeParams> {
        let (Some(account), Some(user), Some(private_key)) =
            (&self.account, &self.user, &self.private_key)
        else {
            bail!("--sql requires --account, --user and --private-key for key-pair auth");
        };
        Ok(SnowflakeParams {
            account: account.clone(),
            user: user.clone(),
            private_key_path: private_key.clone(),
            warehouse: self.warehouse.clone(),
            database: self.database.clone(),
            schema: self.schema.clone(),
            role: self.role.clone(),
        })
    }
}

/// Decide what to show, running the Snowflake query (if any) before the TUI
/// starts so errors print normally instead of inside the alternate screen.
fn build_app(cli: Cli) -> Result<App> {
    if let Some(sql) = &cli.sql {
        let params = cli.snowflake_params()?;
        let batches = snowflake::run_query(&params, sql)?;
        let data = DataTable::from_batches(batches)?;
        let status = format!(
            "snowflake  —  {} rows × {} cols",
            data.num_rows(),
            data.num_cols()
        );
        Ok(App::from_table(data, status))
    } else if let Some(rows) = cli.demo {
        Ok(App::from_demo(rows))
    } else {
        Ok(App::new(cli.path))
    }
}

fn main() -> Result<()> {
    // Pretty-print panics and errors.
    // `ratatui::init()` below layers its own terminal-restoring panic hook on
    // top, so install color_eyre first, then init ratatui.
    color_eyre::install()?;

    let app = build_app(Cli::parse())?;

    // Switch the terminal into raw mode + alternate screen, and always restore
    // it on exit.
    let terminal = ratatui::init();
    let result = app.run(terminal);
    ratatui::restore();
    result
}
