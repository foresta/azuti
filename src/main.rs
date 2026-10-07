mod app;
mod data;
mod query;
mod snowflake;

use app::App;
use clap::Parser;
use color_eyre::eyre::{bail, Result};

use crate::data::DataTable;
use crate::query::QueryEngine;
use crate::snowflake::{ConnConfig, SnowflakeParams};

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

    /// Use a named connection from Snowflake's connections.toml. Explicit flags
    /// above override its values.
    #[arg(long, value_name = "NAME")]
    connection: Option<String>,
    /// Path to connections.toml (default: $SNOWFLAKE_HOME or
    /// ~/.snowflake/connections.toml).
    #[arg(long, value_name = "PATH")]
    connections_file: Option<std::path::PathBuf>,
}

impl Cli {
    /// Assemble Snowflake connection parameters from connections.toml (if any)
    /// overlaid with explicit flags, erroring if a required one is still missing.
    fn snowflake_params(&self) -> Result<SnowflakeParams> {
        let base = self.load_base_connection()?;
        let from_base = |pick: fn(&ConnConfig) -> Option<String>| base.as_ref().and_then(pick);

        let account = self.account.clone().or_else(|| from_base(|b| b.account.clone()));
        let user = self.user.clone().or_else(|| from_base(|b| b.user.clone()));
        let private_key_path = self
            .private_key
            .clone()
            .or_else(|| base.as_ref().and_then(|b| b.private_key()));

        // Point SSO/OAuth connections at the supported auth rather than a vague
        // "missing private key" error.
        if private_key_path.is_none() {
            if let Some(auth) = base.as_ref().and_then(|b| b.authenticator.as_deref()) {
                let a = auth.to_ascii_lowercase();
                if a.contains("externalbrowser") || a.contains("oauth") {
                    bail!("connection uses '{auth}' auth; azuti only supports key-pair (JWT) for now");
                }
            }
        }

        let (Some(account), Some(user), Some(private_key_path)) = (account, user, private_key_path)
        else {
            bail!(
                "missing Snowflake connection details: need account, user and a private key \
                 (via --connection, connections.toml, or --account/--user/--private-key)"
            );
        };

        Ok(SnowflakeParams {
            account,
            user,
            private_key_path,
            warehouse: self.warehouse.clone().or_else(|| from_base(|b| b.warehouse.clone())),
            database: self.database.clone().or_else(|| from_base(|b| b.database.clone())),
            schema: self.schema.clone().or_else(|| from_base(|b| b.schema.clone())),
            role: self.role.clone().or_else(|| from_base(|b| b.role.clone())),
        })
    }

    /// Load the base connection from connections.toml: the `--connection` name if
    /// given, otherwise the `default` connection when no explicit credentials
    /// were passed and the file exists.
    fn load_base_connection(&self) -> Result<Option<ConnConfig>> {
        let file = self
            .connections_file
            .clone()
            .or_else(snowflake::default_connections_file);

        match (&self.connection, file) {
            (Some(name), Some(file)) => Ok(Some(snowflake::load_connection(&file, name)?)),
            (Some(name), None) => {
                bail!("--connection {name} given but no connections.toml path could be determined")
            }
            (None, Some(file))
                if self.account.is_none()
                    && self.user.is_none()
                    && self.private_key.is_none()
                    && file.exists() =>
            {
                // Fall back to the `default` connection, ignoring it if absent.
                Ok(snowflake::load_connection(&file, "default").ok())
            }
            _ => Ok(None),
        }
    }

    /// Whether the user asked for a Snowflake connection at all.
    fn has_snowflake_intent(&self) -> bool {
        self.connection.is_some()
            || self.account.is_some()
            || self.user.is_some()
            || self.private_key.is_some()
    }
}

/// Decide what to show, running the Snowflake query (if any) before the TUI
/// starts so errors print normally instead of inside the alternate screen.
fn build_app(cli: Cli) -> Result<App> {
    if let Some(sql) = &cli.sql {
        // One-shot: run the query now and show the result.
        let params = cli.snowflake_params()?;
        warn_if_region_locator(&params);
        let batches = snowflake::run_query(&params, sql)?;
        let data = DataTable::from_batches(batches)?;
        let status = format!(
            "snowflake  —  {} rows × {} cols",
            data.num_rows(),
            data.num_cols()
        );
        Ok(App::from_table(data, status))
    } else if cli.has_snowflake_intent() {
        // Interactive: open the SQL editor connected to Snowflake.
        let params = cli.snowflake_params()?;
        warn_if_region_locator(&params);
        Ok(App::interactive(QueryEngine::spawn(params)))
    } else if let Some(rows) = cli.demo {
        Ok(App::from_demo(rows))
    } else {
        Ok(App::new(cli.path))
    }
}

/// Key-pair auth puts the account into the JWT, where Snowflake rejects a
/// region/cloud suffix. A dotted account is almost always a region locator.
fn warn_if_region_locator(params: &SnowflakeParams) {
    if params.account.contains('.') {
        eprintln!(
            "warning: account '{}' looks like a region locator; key-pair (JWT) auth needs the \
             organization account identifier (e.g. ORG-ACCOUNT). Find it with: \
             snow sql -q \"SELECT CURRENT_ORGANIZATION_NAME(), CURRENT_ACCOUNT_NAME()\"",
            params.account
        );
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
