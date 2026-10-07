use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, Int32Array, Int64Array, RecordBatch, StringArray, StructArray,
};
use arrow::datatypes::{DataType, Field, Schema};
use chrono::{DateTime, Duration, NaiveTime};
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
    // Key-pair auth puts the account into the JWT, where Snowflake rejects a
    // region/cloud suffix. A dotted account is almost always a region locator,
    // so point the user at the organization account identifier instead.
    if params.account.contains('.') {
        eprintln!(
            "warning: account '{}' looks like a region locator; key-pair (JWT) auth needs the \
             organization account identifier (e.g. ORG-ACCOUNT) — the region suffix makes the \
             JWT invalid. Find it with: \
             snow sql -q \"SELECT CURRENT_ORGANIZATION_NAME(), CURRENT_ACCOUNT_NAME()\"",
            params.account
        );
    }

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
        QueryResult::Arrow(batches) => normalize_batches(batches),
        QueryResult::Empty => Ok(Vec::new()),
        QueryResult::Json(_) => {
            bail!("query did not return Arrow data (non-SELECT statement or a session issue)")
        }
    }
}

/// Snowflake encodes TIMESTAMP columns as a struct `{epoch, fraction[, timezone]}`
/// and TIME as nanoseconds-since-midnight, which the default Arrow formatter
/// renders unreadably. Convert those columns to formatted text. Other columns —
/// including DATE (native `Date32`) — are left untouched.
fn normalize_batches(batches: Vec<RecordBatch>) -> Result<Vec<RecordBatch>> {
    batches.into_iter().map(normalize_batch).collect()
}

fn normalize_batch(batch: RecordBatch) -> Result<RecordBatch> {
    let schema = batch.schema();
    let needs_work = schema
        .fields()
        .iter()
        .any(|f| converter_for(f).is_some());
    if !needs_work {
        return Ok(batch);
    }

    let mut fields: Vec<Field> = Vec::with_capacity(schema.fields().len());
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (i, field) in schema.fields().iter().enumerate() {
        let column = batch.column(i);
        // Fall back to the original column if the shape is unexpected.
        match converter_for(field).and_then(|conv| conv(column)) {
            Some(text) => {
                fields.push(Field::new(field.name(), DataType::Utf8, field.is_nullable()));
                columns.push(text);
            }
            None => {
                fields.push(field.as_ref().clone());
                columns.push(column.clone());
            }
        }
    }

    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
        .wrap_err("rebuilding batch after temporal normalization")
}

/// Pick a converter based on Snowflake's `logicalType` field metadata.
fn converter_for(field: &Field) -> Option<fn(&ArrayRef) -> Option<ArrayRef>> {
    match field.metadata().get("logicalType").map(String::as_str) {
        Some("TIMESTAMP_NTZ") | Some("TIMESTAMP_LTZ") => Some(convert_timestamp_naive),
        Some("TIMESTAMP_TZ") => Some(convert_timestamp_tz),
        Some("TIME") => Some(convert_time),
        _ => None,
    }
}

fn convert_timestamp_naive(column: &ArrayRef) -> Option<ArrayRef> {
    let s = column.as_any().downcast_ref::<StructArray>()?;
    let epoch = s.column_by_name("epoch")?.as_any().downcast_ref::<Int64Array>()?;
    let fraction = s.column_by_name("fraction")?.as_any().downcast_ref::<Int32Array>()?;

    let out = (0..s.len()).map(|i| {
        if s.is_null(i) || epoch.is_null(i) {
            return None;
        }
        let dt = DateTime::from_timestamp(epoch.value(i), fraction.value(i).max(0) as u32)?;
        Some(format_datetime(dt))
    });
    Some(Arc::new(out.collect::<StringArray>()))
}

fn convert_timestamp_tz(column: &ArrayRef) -> Option<ArrayRef> {
    let s = column.as_any().downcast_ref::<StructArray>()?;
    let epoch = s.column_by_name("epoch")?.as_any().downcast_ref::<Int64Array>()?;
    let fraction = s.column_by_name("fraction")?.as_any().downcast_ref::<Int32Array>()?;
    let timezone = s.column_by_name("timezone")?.as_any().downcast_ref::<Int32Array>()?;

    let out = (0..s.len()).map(|i| {
        if s.is_null(i) || epoch.is_null(i) {
            return None;
        }
        let instant = DateTime::from_timestamp(epoch.value(i), fraction.value(i).max(0) as u32)?;
        // Snowflake stores the UTC offset in minutes, biased by +1440.
        let offset_min = timezone.value(i) - 1440;
        let local = instant + Duration::minutes(offset_min as i64);
        let sign = if offset_min < 0 { '-' } else { '+' };
        let abs = offset_min.unsigned_abs();
        Some(format!(
            "{} {sign}{:02}:{:02}",
            format_datetime(local),
            abs / 60,
            abs % 60
        ))
    });
    Some(Arc::new(out.collect::<StringArray>()))
}

fn convert_time(column: &ArrayRef) -> Option<ArrayRef> {
    let a = column.as_any().downcast_ref::<Int64Array>()?;
    let out = (0..a.len()).map(|i| {
        if a.is_null(i) {
            return None;
        }
        let total = a.value(i).max(0);
        let secs = (total / 1_000_000_000) as u32;
        let nanos = (total % 1_000_000_000) as u32;
        let t = NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)?;
        Some(append_fraction(t.format("%H:%M:%S").to_string(), nanos))
    });
    Some(Arc::new(out.collect::<StringArray>()))
}

/// `YYYY-MM-DD HH:MM:SS` with a trimmed fractional part when non-zero. The
/// wall-clock fields of `dt` are used as-is (callers pass the intended local or
/// UTC instant).
fn format_datetime<Tz: chrono::TimeZone>(dt: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let base = dt.format("%Y-%m-%d %H:%M:%S").to_string();
    append_fraction(base, dt.timestamp_subsec_nanos())
}

fn append_fraction(mut s: String, nanos: u32) -> String {
    if nanos > 0 {
        let digits = format!("{nanos:09}");
        s.push('.');
        s.push_str(digits.trim_end_matches('0'));
    }
    s
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

    use arrow::datatypes::Fields;

    fn ts_struct_batch(
        logical: &str,
        epoch: i64,
        fraction: i32,
        timezone: Option<i32>,
    ) -> RecordBatch {
        let mut child_fields = vec![
            Field::new("epoch", DataType::Int64, true),
            Field::new("fraction", DataType::Int32, true),
        ];
        let mut child_arrays: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![epoch])),
            Arc::new(Int32Array::from(vec![fraction])),
        ];
        if let Some(tz) = timezone {
            child_fields.push(Field::new("timezone", DataType::Int32, true));
            child_arrays.push(Arc::new(Int32Array::from(vec![tz])));
        }
        let struct_arr = StructArray::new(Fields::from(child_fields), child_arrays, None);
        let field = Field::new("ts", struct_arr.data_type().clone(), true).with_metadata(
            HashMap::from([("logicalType".to_string(), logical.to_string())]),
        );
        RecordBatch::try_new(Arc::new(Schema::new(vec![field])), vec![Arc::new(struct_arr)]).unwrap()
    }

    fn first_string(batch: &RecordBatch) -> String {
        batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0)
            .to_string()
    }

    #[test]
    fn formats_timestamp_ntz() {
        // 2020-01-02 03:04:05.123 UTC
        let batch = ts_struct_batch("TIMESTAMP_NTZ", 1_577_934_245, 123_000_000, None);
        let out = normalize_batch(batch).unwrap();
        assert_eq!(out.schema().field(0).data_type(), &DataType::Utf8);
        assert_eq!(first_string(&out), "2020-01-02 03:04:05.123");
    }

    #[test]
    fn formats_timestamp_tz_with_offset() {
        // Same instant, +09:00 (offset 540 min, stored biased by +1440 = 1980).
        let batch = ts_struct_batch("TIMESTAMP_TZ", 1_577_934_245, 0, Some(1980));
        let out = normalize_batch(batch).unwrap();
        assert_eq!(first_string(&out), "2020-01-02 12:04:05 +09:00");
    }

    #[test]
    fn formats_time() {
        let ns = (3 * 3600 + 4 * 60 + 5) as i64 * 1_000_000_000;
        let field = Field::new("t", DataType::Int64, true)
            .with_metadata(HashMap::from([("logicalType".to_string(), "TIME".to_string())]));
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![field])),
            vec![Arc::new(Int64Array::from(vec![ns]))],
        )
        .unwrap();
        let out = normalize_batch(batch).unwrap();
        assert_eq!(first_string(&out), "03:04:05");
    }
}
