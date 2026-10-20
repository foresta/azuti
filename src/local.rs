use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use arrow::array::RecordBatch;
use arrow::datatypes::{Field, Schema};
use color_eyre::eyre::Result;
use datafusion::datasource::MemTable;
use datafusion::execution::context::SQLOptions;
use datafusion::prelude::{SessionConfig, SessionContext};

use crate::connection::{Engine, QueryOutcome};

/// Always points at the most recently displayed result.
pub const LAST: &str = "last";

pub struct ResultStore {
    ctx: SessionContext,
}

impl std::fmt::Debug for ResultStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultStore").finish_non_exhaustive()
    }
}

impl Default for ResultStore {
    fn default() -> Self {
        let config = SessionConfig::new().set_str("datafusion.sql_parser.dialect", "snowflake");
        Self {
            ctx: SessionContext::new_with_config(config),
        }
    }
}

impl ResultStore {
    pub fn context(&self) -> SessionContext {
        self.ctx.clone()
    }

    /// Registers an opened file's data. It is registered from the rows already
    /// loaded for display rather than re-read by DataFusion, so SQL sees the
    /// same column types as the grid.
    pub fn register_file(&self, name: &str, batch: RecordBatch) -> Result<()> {
        self.register(name, batch, false)
    }

    /// Registers a displayed result under `name` and as [`LAST`], replacing
    /// either if it already exists.
    pub fn register_result(&self, name: &str, batch: RecordBatch) -> Result<()> {
        self.register(name, batch, true)
    }

    /// Synchronous on the caller's thread: it only wraps shared column arrays,
    /// and finishing before returning means any query submitted afterwards sees
    /// the table.
    fn register(&self, name: &str, batch: RecordBatch, as_last: bool) -> Result<()> {
        let batch = sql_friendly_columns(batch)?;
        let table = Arc::new(MemTable::try_new(batch.schema(), vec![vec![batch]])?);
        let aliases: &[&str] = if as_last { &[name, LAST] } else { &[name] };
        for alias in aliases {
            self.ctx.deregister_table(*alias)?;
            self.ctx.register_table(*alias, table.clone())?;
        }
        Ok(())
    }
}

pub struct LocalEngine {
    sql_tx: Sender<String>,
    result_rx: Receiver<QueryOutcome>,
    _handle: JoinHandle<()>,
}

impl std::fmt::Debug for LocalEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEngine").finish_non_exhaustive()
    }
}

impl LocalEngine {
    pub fn spawn(ctx: SessionContext) -> Self {
        let (sql_tx, sql_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let handle = thread::spawn(move || worker(ctx, sql_rx, result_tx));
        Self {
            sql_tx,
            result_rx,
            _handle: handle,
        }
    }
}

impl Engine for LocalEngine {
    fn submit(&self, sql: String) {
        let _ = self.sql_tx.send(sql);
    }

    fn poll(&self) -> Option<QueryOutcome> {
        self.result_rx.try_recv().ok()
    }
}

fn worker(ctx: SessionContext, sql_rx: Receiver<String>, result_tx: Sender<QueryOutcome>) {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = result_tx.send(Err(format!("starting local SQL: {e}")));
            return;
        }
    };
    // Read-only: a CREATE / DROP / COPY would change what is registered (or
    // write files) behind the catalog's back.
    let options = SQLOptions::new()
        .with_allow_ddl(false)
        .with_allow_dml(false)
        .with_allow_statements(false);
    while let Ok(sql) = sql_rx.recv() {
        let outcome = runtime
            .block_on(async { ctx.sql_with_options(&sql, options).await?.collect().await })
            .map_err(|e| e.to_string());
        if result_tx.send(outcome).is_err() {
            return;
        }
    }
}

/// DataFusion lower-cases unquoted identifiers, so an upper-case column from
/// Snowflake (`ORDER_ID`) would need quoting in every query: columns spelled
/// the way Snowflake folds unquoted names are renamed to lower case, unless
/// that form is already taken. A table with two equal column names (a
/// `SELECT *` over a join) cannot be queried at all, so repeats get `_2`,
/// `_3`, …. Only the schema changes: the column arrays are shared, not copied.
fn sql_friendly_columns(batch: RecordBatch) -> Result<RecordBatch> {
    let schema = batch.schema();
    let original: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    let folded = |n: &str| {
        n.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
            && n.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '$')
    };
    let mut used = std::collections::HashSet::new();
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|f| {
            let lower = f.name().to_ascii_lowercase();
            let base = if folded(f.name()) && !original.contains(&lower.as_str()) {
                lower
            } else {
                f.name().clone()
            };
            let mut name = base.clone();
            let mut n = 1;
            while !used.insert(name.clone()) {
                n += 1;
                name = format!("{base}_{n}");
            }
            f.as_ref().clone().with_name(name)
        })
        .collect();
    Ok(RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        batch.columns().to_vec(),
    )?)
}

/// `Sales-2024.csv` → `sales_2024`: lower-case so the unquoted name DataFusion
/// normalizes to finds it.
pub fn table_name_for(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("data");
    let mut name: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if name.chars().all(|c| c == '_') {
        name = "file".to_string();
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    // Results own `last` and `result_N`; a file must not shadow them.
    let is_result = name
        .strip_prefix("result_")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    if name == LAST || is_result {
        name.insert_str(0, "file_");
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::DataTable;
    use std::path::PathBuf;
    use std::time::Duration;

    fn sample() -> RecordBatch {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/sample.csv");
        DataTable::load(&path).unwrap().batch().clone()
    }

    fn query(engine: &LocalEngine, sql: &str) -> QueryOutcome {
        engine.submit(sql.to_string());
        for _ in 0..1000 {
            if let Some(outcome) = engine.poll() {
                return outcome;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("no result for {sql}");
    }

    fn rows(engine: &LocalEngine, sql: &str) -> usize {
        let batches = query(engine, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        batches.iter().map(|b| b.num_rows()).sum()
    }

    fn with_sample() -> (ResultStore, LocalEngine) {
        let store = ResultStore::default();
        store.register_file("sample", sample()).unwrap();
        let engine = LocalEngine::spawn(store.context());
        (store, engine)
    }

    #[test]
    fn table_names_are_lower_case_identifiers() {
        assert_eq!(table_name_for(Path::new("dir/Sales-2024.csv")), "sales_2024");
        assert_eq!(table_name_for(Path::new("2024.parquet")), "_2024");
        assert_eq!(table_name_for(Path::new("plain.csv")), "plain");
        assert_eq!(table_name_for(Path::new("売上.csv")), "file");
        assert_eq!(table_name_for(Path::new("last.csv")), "file_last");
        assert_eq!(table_name_for(Path::new("result_3.csv")), "file_result_3");
    }

    #[test]
    fn queries_a_registered_file_by_its_name_in_any_case() {
        let (_store, engine) = with_sample();
        assert_eq!(rows(&engine, "SELECT * FROM sample WHERE city = 'Boston'"), 2);
        assert_eq!(rows(&engine, "SELECT * FROM SAMPLE LIMIT 1"), 1);
    }

    #[test]
    fn a_result_is_reachable_as_result_n_and_last_but_a_file_is_not_last() {
        let (store, engine) = with_sample();
        assert!(query(&engine, "SELECT * FROM last").is_err());

        let first = query(&engine, "SELECT id FROM sample").unwrap();
        let total: usize = first.iter().map(|b| b.num_rows()).sum();
        store.register_result("result_1", first[0].clone()).unwrap();
        assert_eq!(rows(&engine, "SELECT * FROM result_1"), total);
        assert_eq!(rows(&engine, "SELECT * FROM last"), total);

        let one = query(&engine, "SELECT id FROM sample LIMIT 1").unwrap();
        store.register_result("result_2", one[0].clone()).unwrap();
        assert_eq!(rows(&engine, "SELECT * FROM last"), 1);
        assert_eq!(rows(&engine, "SELECT * FROM result_1"), total);
    }

    #[test]
    fn upper_case_snowflake_columns_are_queryable_unquoted() {
        use arrow::array::{ArrayRef, Int32Array};
        use arrow::datatypes::DataType;
        let schema = Schema::new(vec![
            Field::new("ORDER_ID", DataType::Int32, false),
            Field::new("Mixed", DataType::Int32, false),
        ]);
        let col: ArrayRef = Arc::new(Int32Array::from(vec![1, 2]));
        let batch = RecordBatch::try_new(Arc::new(schema), vec![col.clone(), col]).unwrap();
        let store = ResultStore::default();
        store.register_result("result_1", batch).unwrap();
        let engine = LocalEngine::spawn(store.context());
        assert_eq!(rows(&engine, "SELECT order_id FROM last WHERE ORDER_ID > 1"), 1);
        assert_eq!(rows(&engine, "SELECT \"Mixed\" FROM last"), 2);
    }

    #[test]
    fn repeated_column_names_get_numbered() {
        use arrow::array::{ArrayRef, Int32Array};
        use arrow::datatypes::DataType;
        let schema = Schema::new(vec![
            Field::new("ID", DataType::Int32, false),
            Field::new("ID", DataType::Int32, false),
            Field::new("id_2", DataType::Int32, false),
        ]);
        let col: ArrayRef = Arc::new(Int32Array::from(vec![1]));
        let batch =
            RecordBatch::try_new(Arc::new(schema), vec![col.clone(), col.clone(), col]).unwrap();
        let store = ResultStore::default();
        store.register_result("result_1", batch).unwrap();
        let engine = LocalEngine::spawn(store.context());
        let out = query(&engine, "SELECT * FROM last").unwrap();
        let names: Vec<_> = out[0].schema().fields().iter().map(|f| f.name().clone()).collect();
        assert_eq!(names, ["id", "id_2", "id_2_2"]);
    }

    #[test]
    fn local_source_lists_registered_tables_and_their_columns() {
        use crate::catalog::source::{LocalSource, MetadataSource};
        use crate::catalog::{Children, Name, Scope};
        let (store, _engine) = with_sample();
        let mut source = LocalSource::new(store.context()).unwrap();
        let (_, current) = source.identify().unwrap();
        let (db, schema) = (current.database.unwrap(), current.schema.unwrap());
        let Children::Relations(rels) = source.fetch(&Scope::Schema(db.clone(), schema.clone())).unwrap()
        else {
            panic!("expected relations");
        };
        assert_eq!(rels.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["sample"]);
        let Children::Columns(cols) =
            source.fetch(&Scope::Relation(db, schema, Name::new("sample"))).unwrap()
        else {
            panic!("expected columns");
        };
        assert_eq!(cols[0].name, Name::new("id"));
    }

    #[test]
    fn statements_that_change_state_are_rejected() {
        let (_store, engine) = with_sample();
        assert!(query(&engine, "DROP TABLE sample").is_err());
        assert!(query(&engine, "CREATE TABLE t AS SELECT 1").is_err());
        assert_eq!(rows(&engine, "SELECT * FROM sample LIMIT 1"), 1);
    }
}
