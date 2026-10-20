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

use crate::query::QueryOutcome;

/// Always points at the most recently displayed result.
pub const LAST: &str = "last";

enum LocalCommand {
    Register {
        name: String,
        batch: RecordBatch,
        as_last: bool,
    },
    Query(String),
}

#[derive(Debug)]
pub enum LocalEvent {
    Registered,
    RegisterFailed { name: String, error: String },
    Result(QueryOutcome),
}

/// Runs DataFusion on its own thread. Registrations and queries share one
/// queue, so a query always sees every table registered before it.
pub struct LocalEngine {
    cmd_tx: Sender<LocalCommand>,
    event_rx: Receiver<LocalEvent>,
    ctx: SessionContext,
    _handle: JoinHandle<()>,
}

impl std::fmt::Debug for LocalEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEngine").finish_non_exhaustive()
    }
}

impl LocalEngine {
    pub fn spawn() -> Self {
        let config = SessionConfig::new().set_str("datafusion.sql_parser.dialect", "snowflake");
        let ctx = SessionContext::new_with_config(config);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let worker_ctx = ctx.clone();
        let handle = thread::spawn(move || worker(worker_ctx, cmd_rx, event_tx));
        Self {
            cmd_tx,
            event_rx,
            ctx,
            _handle: handle,
        }
    }

    /// Registers an opened file's data. It is registered from the rows already
    /// loaded for display rather than re-read by DataFusion, so SQL sees the
    /// same column types as the grid.
    pub fn register_file(&self, name: &str, batch: RecordBatch) {
        self.register(name, batch, false);
    }

    /// Registers a displayed result under `name` and as [`LAST`], replacing
    /// either if it already exists.
    pub fn register_result(&self, name: &str, batch: RecordBatch) {
        self.register(name, batch, true);
    }

    fn register(&self, name: &str, batch: RecordBatch, as_last: bool) {
        let _ = self.cmd_tx.send(LocalCommand::Register {
            name: name.to_string(),
            batch,
            as_last,
        });
    }

    pub fn submit(&self, sql: String) {
        let _ = self.cmd_tx.send(LocalCommand::Query(sql));
    }

    pub fn poll(&self) -> Vec<LocalEvent> {
        self.event_rx.try_iter().collect()
    }

    pub fn context(&self) -> SessionContext {
        self.ctx.clone()
    }
}

fn worker(ctx: SessionContext, cmd_rx: Receiver<LocalCommand>, event_tx: Sender<LocalEvent>) {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = event_tx.send(LocalEvent::Result(Err(format!("starting local SQL: {e}"))));
            return;
        }
    };
    // Read-only: a CREATE / DROP / COPY would change what is registered (or
    // write files) behind the catalog's back.
    let options = SQLOptions::new()
        .with_allow_ddl(false)
        .with_allow_dml(false)
        .with_allow_statements(false);
    while let Ok(cmd) = cmd_rx.recv() {
        let event = match cmd {
            LocalCommand::Register {
                name,
                batch,
                as_last,
            } => match register(&ctx, &name, batch, as_last) {
                Ok(()) => LocalEvent::Registered,
                Err(e) => LocalEvent::RegisterFailed {
                    name,
                    error: format!("{e:#}"),
                },
            },
            LocalCommand::Query(sql) => LocalEvent::Result(
                runtime
                    .block_on(async { ctx.sql_with_options(&sql, options).await?.collect().await })
                    .map_err(|e| e.to_string()),
            ),
        };
        if event_tx.send(event).is_err() {
            return;
        }
    }
}

fn register(ctx: &SessionContext, name: &str, batch: RecordBatch, as_last: bool) -> Result<()> {
    let batch = sql_friendly_columns(batch)?;
    let table = Arc::new(MemTable::try_new(batch.schema(), vec![vec![batch]])?);
    let aliases: &[&str] = if as_last { &[name, LAST] } else { &[name] };
    for alias in aliases {
        ctx.deregister_table(*alias)?;
        ctx.register_table(*alias, table.clone())?;
    }
    Ok(())
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

    fn next(engine: &LocalEngine) -> LocalEvent {
        for _ in 0..1000 {
            if let Some(e) = engine.poll().into_iter().next() {
                return e;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("no event from the local engine");
    }

    fn query(engine: &LocalEngine, sql: &str) -> std::result::Result<Vec<RecordBatch>, String> {
        engine.submit(sql.to_string());
        match next(engine) {
            LocalEvent::Result(r) => r,
            other => panic!("{sql}: {other:?}"),
        }
    }

    fn rows(engine: &LocalEngine, sql: &str) -> usize {
        let batches = query(engine, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        batches.iter().map(|b| b.num_rows()).sum()
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
        let engine = LocalEngine::spawn();
        engine.register_file("sample", sample());
        assert!(matches!(next(&engine), LocalEvent::Registered));
        assert_eq!(rows(&engine, "SELECT * FROM sample WHERE city = 'Boston'"), 2);
        assert_eq!(rows(&engine, "SELECT * FROM SAMPLE LIMIT 1"), 1);
    }

    #[test]
    fn a_result_is_reachable_as_result_n_and_last_but_a_file_is_not_last() {
        let engine = LocalEngine::spawn();
        engine.register_file("sample", sample());
        next(&engine);
        assert!(query(&engine, "SELECT * FROM last").is_err());

        let first = query(&engine, "SELECT id FROM sample").unwrap();
        let total: usize = first.iter().map(|b| b.num_rows()).sum();
        engine.register_result("result_1", first[0].clone());
        next(&engine);
        assert_eq!(rows(&engine, "SELECT * FROM result_1"), total);
        assert_eq!(rows(&engine, "SELECT * FROM last"), total);

        let one = query(&engine, "SELECT id FROM sample LIMIT 1").unwrap();
        engine.register_result("result_2", one[0].clone());
        next(&engine);
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
        let engine = LocalEngine::spawn();
        engine.register_result("result_1", batch);
        next(&engine);
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
        let engine = LocalEngine::spawn();
        engine.register_result("result_1", batch);
        assert!(matches!(next(&engine), LocalEvent::Registered));
        let out = query(&engine, "SELECT * FROM last").unwrap();
        let names: Vec<_> = out[0].schema().fields().iter().map(|f| f.name().clone()).collect();
        assert_eq!(names, ["id", "id_2", "id_2_2"]);
    }

    #[test]
    fn local_source_lists_registered_tables_and_their_columns() {
        use crate::catalog::source::{LocalSource, MetadataSource};
        use crate::catalog::{Children, Name, Scope};
        let engine = LocalEngine::spawn();
        engine.register_file("sample", sample());
        next(&engine);
        let mut source = LocalSource::new(engine.context()).unwrap();
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
        let engine = LocalEngine::spawn();
        engine.register_file("sample", sample());
        next(&engine);
        assert!(query(&engine, "DROP TABLE sample").is_err());
        assert!(query(&engine, "CREATE TABLE t AS SELECT 1").is_err());
        assert_eq!(rows(&engine, "SELECT * FROM sample LIMIT 1"), 1);
    }
}
