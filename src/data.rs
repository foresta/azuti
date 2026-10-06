use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::compute::concat_batches;
use arrow::csv::reader::Format;
use arrow::csv::ReaderBuilder;
use arrow::datatypes::SchemaRef;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use color_eyre::eyre::{bail, Context, Result};

/// A table loaded from a local file: a schema plus all rows as a single Arrow
/// `RecordBatch`.
///
/// For step b we keep the data as Arrow but format every cell up front (see
/// [`DataTable::to_string_rows`]). That is the naive baseline; step c replaces
/// it with viewport rendering that formats only the cells currently on screen.
#[derive(Debug)]
pub struct DataTable {
    schema: SchemaRef,
    batch: RecordBatch,
}

impl DataTable {
    /// Load a `.csv` or `.parquet` file, dispatching on the extension.
    pub fn load(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        let (schema, batches) = match ext.as_str() {
            "csv" => load_csv(path)?,
            "parquet" => load_parquet(path)?,
            "" => bail!("missing file extension (expected .csv or .parquet)"),
            other => bail!("unsupported file type: .{other} (expected .csv or .parquet)"),
        };

        // Collapse the per-read batches into one for simple indexing.
        let batch = concat_batches(&schema, &batches).wrap_err("concatenating record batches")?;
        Ok(Self { schema, batch })
    }

    pub fn column_names(&self) -> Vec<String> {
        self.schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    pub fn num_rows(&self) -> usize {
        self.batch.num_rows()
    }

    pub fn num_cols(&self) -> usize {
        self.batch.num_columns()
    }

    /// Format the whole table into rows of owned strings.
    ///
    /// One [`ArrayFormatter`] per column (type-erased, created once), then each
    /// cell is formatted by row index. Naive for step b; step c formats only the
    /// visible window.
    pub fn to_string_rows(&self) -> Result<Vec<Vec<String>>> {
        let options = FormatOptions::default().with_null("");
        let formatters: Vec<ArrayFormatter> = self
            .batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
            .collect::<std::result::Result<_, _>>()
            .wrap_err("building column formatters")?;

        let mut rows = Vec::with_capacity(self.batch.num_rows());
        for r in 0..self.batch.num_rows() {
            let row = formatters.iter().map(|f| f.value(r).to_string()).collect();
            rows.push(row);
        }
        Ok(rows)
    }
}

fn load_csv(path: &Path) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    // Infer the schema from a sample of rows (assuming a header row), then read.
    let format = Format::default().with_header(true);
    let infer_reader = BufReader::new(File::open(path).wrap_err_with(|| open_msg(path))?);
    let (schema, _records) = format
        .infer_schema(infer_reader, Some(1000))
        .wrap_err("inferring CSV schema")?;
    let schema = Arc::new(schema);

    let file = BufReader::new(File::open(path).wrap_err_with(|| open_msg(path))?);
    let reader = ReaderBuilder::new(schema.clone())
        .with_header(true)
        .build(file)
        .wrap_err("building CSV reader")?;

    let batches = reader
        .collect::<std::result::Result<Vec<_>, _>>()
        .wrap_err("reading CSV")?;
    Ok((schema, batches))
}

fn load_parquet(path: &Path) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = File::open(path).wrap_err_with(|| open_msg(path))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).wrap_err("opening Parquet file")?;
    let schema = builder.schema().clone();
    let reader = builder.build().wrap_err("building Parquet reader")?;

    let batches = reader
        .collect::<std::result::Result<Vec<_>, _>>()
        .wrap_err("reading Parquet")?;
    Ok((schema, batches))
}

fn open_msg(path: &Path) -> String {
    format!("opening {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn testdata(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name)
    }

    #[test]
    fn loads_csv_sample() {
        let table = DataTable::load(&testdata("sample.csv")).unwrap();
        assert_eq!(table.num_rows(), 10);
        assert_eq!(table.num_cols(), 5);
        assert_eq!(
            table.column_names(),
            ["id", "name", "role", "city", "score"]
        );

        let rows = table.to_string_rows().unwrap();
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0][1], "Ada Lovelace");
    }

    #[test]
    fn rejects_unsupported_extension() {
        // Dispatch happens on the extension before the file is opened, so this
        // need not exist.
        let err = DataTable::load(&PathBuf::from("foo.txt")).unwrap_err();
        assert!(err.to_string().contains("unsupported file type"));
    }
}
