use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::compute::concat_batches;
use arrow::csv::reader::Format;
use arrow::csv::ReaderBuilder;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use color_eyre::eyre::{bail, Context, Result};
use unicode_width::UnicodeWidthStr;

/// A table loaded from a local file or generated synthetically: a schema plus
/// all rows as a single Arrow `RecordBatch`.
///
/// Cells are formatted to text on demand (see [`DataTable::formatters`]), so the
/// view can render just the rows currently on screen.
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

    /// Build one [`ArrayFormatter`] per column. The formatters borrow the
    /// underlying Arrow arrays, so cells are rendered to text on demand without
    /// copying the column data — this is what makes viewport rendering cheap.
    pub fn formatters(&self) -> Result<Vec<ArrayFormatter<'_>>> {
        let options = FormatOptions::default().with_null("");
        self.batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
            .collect::<std::result::Result<_, _>>()
            .wrap_err("building column formatters")
    }

    /// Format a single row across all columns. Used for the detail overlay.
    pub fn format_row(&self, row: usize) -> Result<Vec<String>> {
        if row >= self.num_rows() {
            return Ok(Vec::new());
        }
        let formatters = self.formatters()?;
        Ok(formatters
            .iter()
            .map(|f| f.value(row).to_string())
            .collect())
    }

    /// Maximum display width (terminal cells) per column, measured from the
    /// header name plus the first `sample` rows. Only the sample is scanned so
    /// this stays cheap on large tables.
    pub fn column_display_widths(&self, sample: usize) -> Result<Vec<usize>> {
        let formatters = self.formatters()?;
        let mut widths: Vec<usize> = self.column_names().iter().map(|n| n.width()).collect();
        let scan = self.num_rows().min(sample);
        for r in 0..scan {
            for (c, f) in formatters.iter().enumerate() {
                let w = f.value(r).to_string().width();
                if w > widths[c] {
                    widths[c] = w;
                }
            }
        }
        Ok(widths)
    }

    /// Build a synthetic table of `nrows` rows for stress-testing the viewer.
    pub fn demo(nrows: usize) -> Self {
        let ids = Int64Array::from_iter_values(0..nrows as i64);
        let names = StringArray::from_iter_values((0..nrows).map(|i| format!("item-{i}")));
        const CATS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
        let categories = StringArray::from_iter_values((0..nrows).map(|i| CATS[i % CATS.len()]));
        let values = Float64Array::from_iter_values((0..nrows).map(|i| i as f64 * 1.5));

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("category", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(ids),
                Arc::new(names),
                Arc::new(categories),
                Arc::new(values),
            ],
        )
        .expect("synthetic columns match the schema");
        Self { schema, batch }
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

        let row0 = table.format_row(0).unwrap();
        assert_eq!(row0.len(), 5);
        assert_eq!(row0[1], "Ada Lovelace");
    }

    #[test]
    fn rejects_unsupported_extension() {
        // Dispatch happens on the extension before the file is opened, so this
        // need not exist.
        let err = DataTable::load(&PathBuf::from("foo.txt")).unwrap_err();
        assert!(err.to_string().contains("unsupported file type"));
    }
}
