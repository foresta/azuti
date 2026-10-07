use std::collections::HashMap;
use std::path::{Path, PathBuf};

use arrow::array::RecordBatch;
use color_eyre::eyre::{bail, eyre, Context, Result};
use serde::Deserialize;
use snowflake_api::{QueryResult, SnowflakeApi};

/// One connection entry from Snowflake's `connections.toml` (the same file the
/// `snow` CLI uses). All fields are optional; only the ones needed for key-pair
/// auth are read.
#[derive(Debug, Default, Deserialize)]
pub struct ConnConfig {
    pub account: Option<String>,
    pub user: Option<String>,
    pub private_key_path: Option<String>,
    pub private_key_file: Option<String>,
    pub warehouse: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub role: Option<String>,
    pub authenticator: Option<String>,
}

impl ConnConfig {
    /// The private key path, from `private_key_path` or `private_key_file`, with
    /// a leading `~/` expanded.
    pub fn private_key(&self) -> Option<PathBuf> {
        self.private_key_path
            .as_ref()
            .or(self.private_key_file.as_ref())
            .map(|s| expand_tilde(s))
    }
}

/// Default location of `connections.toml`: `$SNOWFLAKE_HOME/connections.toml`,
/// else `~/.snowflake/connections.toml`.
pub fn default_connections_file() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("SNOWFLAKE_HOME") {
        return Some(PathBuf::from(home).join("connections.toml"));
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".snowflake").join("connections.toml"))
}

/// Load one named connection from a `connections.toml` file.
pub fn load_connection(path: &Path, name: &str) -> Result<ConnConfig> {
    let text =
        std::fs::read_to_string(path).wrap_err_with(|| format!("reading {}", path.display()))?;
    let mut conns: HashMap<String, ConnConfig> =
        toml::from_str(&text).wrap_err_with(|| format!("parsing {}", path.display()))?;
    conns.remove(name).ok_or_else(|| {
        let mut names: Vec<_> = conns.keys().cloned().collect();
        names.sort();
        eyre!(
            "connection '{name}' not found in {} (available: {})",
            path.display(),
            names.join(", ")
        )
    })
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn loads_named_connection() {
        let dir = std::env::temp_dir().join(format!("azuti-conn-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connections.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            concat!(
                "[default]\n",
                "account = \"ab12345.ap-northeast-1.aws\"\n",
                "user = \"ME\"\n",
                "private_key_path = \"~/key.p8\"\n",
                "warehouse = \"WH\"\n",
                "role = \"RO\"\n",
            )
        )
        .unwrap();

        let c = load_connection(&path, "default").unwrap();
        assert_eq!(c.account.as_deref(), Some("ab12345.ap-northeast-1.aws"));
        assert_eq!(c.user.as_deref(), Some("ME"));
        assert_eq!(c.warehouse.as_deref(), Some("WH"));
        assert!(c
            .private_key()
            .unwrap()
            .to_string_lossy()
            .ends_with("key.p8"));

        let err = load_connection(&path, "missing").unwrap_err();
        assert!(err.to_string().contains("not found"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
