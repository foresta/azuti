use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, Decimal128Array, Int32Array, Int64Array, RecordBatch, StringArray,
    StructArray,
};
use arrow::datatypes::{DataType, Decimal128Type, Field, Schema};
use chrono::{DateTime, Duration, NaiveTime};
use color_eyre::eyre::{bail, eyre, Context, Result};
use serde::Deserialize;
use snowflake_api::{JsonResult, QueryResult, SnowflakeApi};

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
#[derive(Debug, Clone)]
pub struct SnowflakeParams {
    pub account: String,
    pub user: String,
    pub private_key_path: PathBuf,
    pub warehouse: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub role: Option<String>,
}

/// What a statement returned.
#[derive(Debug)]
pub enum QueryOutput {
    Arrow(Vec<RecordBatch>),
    /// Results Snowflake sends as a JSON rowset, such as `SHOW` and `DESCRIBE`.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    },
    /// Zero rows. snowflake-api drops the column list in this case.
    Empty,
}

impl QueryOutput {
    /// Convert to record batches for the result grid. JSON rows become an
    /// all-Utf8 batch.
    pub fn into_batches(self) -> Result<Vec<RecordBatch>> {
        match self {
            QueryOutput::Arrow(batches) => Ok(batches),
            QueryOutput::Empty => Ok(Vec::new()),
            QueryOutput::Rows { columns, rows } => {
                let fields: Vec<Field> = columns
                    .iter()
                    .map(|c| Field::new(c, DataType::Utf8, true))
                    .collect();
                let arrays: Vec<ArrayRef> = (0..columns.len())
                    .map(|i| {
                        let col: StringArray =
                            rows.iter().map(|r| r.get(i).cloned().flatten()).collect();
                        Arc::new(col) as ArrayRef
                    })
                    .collect();
                let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
                    .wrap_err("building a batch from JSON rows")?;
                Ok(vec![batch])
            }
        }
    }
}

/// A logged-in Snowflake session, reused across statements.
pub struct Session {
    api: SnowflakeApi,
    runtime: tokio::runtime::Runtime,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    /// Build the client with key-pair auth. snowflake-api logs in lazily on the
    /// first statement and renews tokens itself.
    pub fn connect(params: &SnowflakeParams) -> Result<Self> {
        let private_key_pem =
            std::fs::read_to_string(&params.private_key_path).wrap_err_with(|| {
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
        Ok(Self { api, runtime })
    }

    pub fn exec(&self, sql: &str) -> Result<QueryOutput> {
        let result = self
            .runtime
            .block_on(self.api.exec(sql))
            .wrap_err("executing the query")?;
        match result {
            QueryResult::Arrow(batches) => Ok(QueryOutput::Arrow(normalize_batches(batches)?)),
            QueryResult::Empty => Ok(QueryOutput::Empty),
            QueryResult::Json(json) => json_rows(json),
        }
    }
}

fn json_rows(json: JsonResult) -> Result<QueryOutput> {
    let columns = json.schema.iter().map(|f| f.name.clone()).collect();
    let rows = json_cells(&json.value)?;
    Ok(QueryOutput::Rows { columns, rows })
}

fn json_cells(value: &serde_json::Value) -> Result<Vec<Vec<Option<String>>>> {
    let Some(rows) = value.as_array() else {
        bail!("unexpected JSON rowset shape: {value}");
    };
    rows.iter()
        .map(|row| {
            let Some(cells) = row.as_array() else {
                bail!("unexpected JSON row shape: {row}");
            };
            Ok(cells
                .iter()
                .map(|v| match v {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                })
                .collect())
        })
        .collect()
}

/// Connect, run `sql` once, and return the result as record batches.
pub fn run_query(params: &SnowflakeParams, sql: &str) -> Result<Vec<RecordBatch>> {
    Session::connect(params)?.exec(sql)?.into_batches()
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
        match converter_for(field).and_then(|conv| conv(field, column)) {
            Some(converted) => {
                fields.push(Field::new(
                    field.name(),
                    converted.data_type().clone(),
                    field.is_nullable(),
                ));
                columns.push(converted);
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

type Converter = fn(&Field, &ArrayRef) -> Option<ArrayRef>;

/// Pick a converter based on Snowflake's `logicalType` field metadata.
fn converter_for(field: &Field) -> Option<Converter> {
    match field.metadata().get("logicalType").map(String::as_str) {
        Some("TIMESTAMP_NTZ") | Some("TIMESTAMP_LTZ") => Some(|_, c| convert_timestamp_naive(c)),
        Some("TIMESTAMP_TZ") => Some(|_, c| convert_timestamp_tz(c)),
        Some("TIME") => Some(|_, c| convert_time(c)),
        Some("FIXED") if field.data_type().is_integer() && scale_of(field) > 0 => {
            Some(convert_scaled_fixed)
        }
        _ => None,
    }
}

fn scale_of(field: &Field) -> i8 {
    field
        .metadata()
        .get("scale")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Snowflake sends a small NUMBER(p, s) as plain integers holding the value
/// times 10^s, with the scale only in the metadata. A value-preserving cast to
/// Decimal would multiply again, so the integers are reinterpreted instead.
fn convert_scaled_fixed(field: &Field, column: &ArrayRef) -> Option<ArrayRef> {
    let precision = field
        .metadata()
        .get("precision")
        .and_then(|s| s.parse().ok())
        .unwrap_or(38);
    let ints = arrow::compute::cast(column, &DataType::Int64).ok()?;
    let ints = ints.as_any().downcast_ref::<Int64Array>()?;
    let decimals: Decimal128Array = ints
        .unary::<_, Decimal128Type>(i128::from)
        .with_precision_and_scale(precision, scale_of(field))
        .ok()?;
    Some(Arc::new(decimals))
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
    fn scaled_integers_become_decimals_with_the_same_value() {
        let meta = HashMap::from([
            ("logicalType".to_string(), "FIXED".to_string()),
            ("precision".to_string(), "10".to_string()),
            ("scale".to_string(), "2".to_string()),
        ]);
        let field = Field::new("X", DataType::Int16, true).with_metadata(meta);
        let column: ArrayRef =
            Arc::new(arrow::array::Int16Array::from(vec![Some(125), None, Some(-125)]));
        let batch =
            RecordBatch::try_new(Arc::new(Schema::new(vec![field])), vec![column]).unwrap();
        let out = normalize_batch(batch).unwrap();
        assert_eq!(out.schema().field(0).data_type(), &DataType::Decimal128(10, 2));
        let shown = arrow::util::display::array_value_to_string(out.column(0), 0).unwrap();
        assert_eq!(shown, "1.25");
        assert!(out.column(0).is_null(1));
        let negative = arrow::util::display::array_value_to_string(out.column(0), 2).unwrap();
        assert_eq!(negative, "-1.25");
    }

    #[test]
    fn large_scaled_integers_keep_every_digit() {
        let meta = HashMap::from([
            ("logicalType".to_string(), "FIXED".to_string()),
            ("precision".to_string(), "38".to_string()),
            ("scale".to_string(), "3".to_string()),
        ]);
        let field = Field::new("X", DataType::Int64, true).with_metadata(meta);
        let column: ArrayRef = Arc::new(Int64Array::from(vec![i64::MAX]));
        let batch =
            RecordBatch::try_new(Arc::new(Schema::new(vec![field])), vec![column]).unwrap();
        let out = normalize_batch(batch).unwrap();
        let shown = arrow::util::display::array_value_to_string(out.column(0), 0).unwrap();
        assert_eq!(shown, "9223372036854775.807");
    }

    #[test]
    fn json_rowset_cells_keep_nulls_and_stringify_other_values() {
        let value = serde_json::json!([["a", null, 3], ["b", "x", true]]);
        let rows = json_cells(&value).unwrap();
        assert_eq!(rows[0], vec![Some("a".into()), None, Some("3".into())]);
        assert_eq!(rows[1][2].as_deref(), Some("true"));
    }

    #[test]
    fn json_rows_become_a_utf8_batch_in_column_order() {
        let out = QueryOutput::Rows {
            columns: vec!["name".into(), "comment".into()],
            rows: vec![vec![Some("T".into()), None]],
        };
        let batches = out.into_batches().unwrap();
        let schema = batches[0].schema();
        assert_eq!(schema.field(0).name(), "name");
        assert_eq!(schema.field(1).name(), "comment");
        assert!(batches[0].column(1).is_null(0));
    }

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
