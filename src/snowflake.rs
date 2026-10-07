use std::path::PathBuf;

use arrow::array::RecordBatch;
use color_eyre::eyre::{bail, Context, Result};
use snowflake_api::{QueryResult, SnowflakeApi};

/// Connection parameters for key-pair (JWT) authentication.
pub struct SnowflakeParams {
    pub account: String,
    pub user: String,
    pub private_key_path: PathBuf,
    pub warehouse: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub role: Option<String>,
}

/// Connect to Snowflake with key-pair auth, run `sql`, and return the result as
/// Arrow record batches.
///
/// The query runs once, synchronously (on a short-lived Tokio runtime), before
/// the TUI starts — so connection or SQL errors surface on the terminal rather
/// than inside the alternate screen.
pub fn run_query(params: &SnowflakeParams, sql: &str) -> Result<Vec<RecordBatch>> {
    let private_key_pem = std::fs::read_to_string(&params.private_key_path).wrap_err_with(|| {
        format!("reading private key {}", params.private_key_path.display())
    })?;

    let api = SnowflakeApi::with_certificate_auth(
        &params.account,
        params.warehouse.as_deref(),
        params.database.as_deref(),
        params.schema.as_deref(),
        &params.user,
        params.role.as_deref(),
        &private_key_pem,
    )
    .wrap_err("initializing the Snowflake connection")?;

    let runtime = tokio::runtime::Runtime::new().wrap_err("creating the async runtime")?;
    let result = runtime
        .block_on(api.exec(sql))
        .wrap_err("executing the query")?;

    match result {
        QueryResult::Arrow(batches) => Ok(batches),
        QueryResult::Empty => Ok(Vec::new()),
        QueryResult::Json(_) => {
            bail!("query did not return Arrow data (non-SELECT statement or a session issue)")
        }
    }
}
