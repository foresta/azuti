use std::collections::HashSet;

use arrow::array::{Array, AsArray};
use color_eyre::eyre::{bail, eyre, Result};

use super::{
    CatalogKey, Children, Column, CurrentScope, Database, Folding, Name, Relation, RelationKind,
    Schema, Scope,
};
use std::sync::Arc;

use datafusion::catalog::SchemaProvider;
use datafusion::prelude::SessionContext;

use crate::snowflake::{QueryOutput, Session};

pub trait MetadataSource {
    fn identify(&mut self) -> Result<(CatalogKey, CurrentScope)>;
    fn fetch(&mut self, scope: &Scope) -> Result<Children>;
    fn folding(&self) -> Folding {
        Folding::Upper
    }
}

/// Each page is far below SHOW's 10,000-row cap in practice: snowflake-api reads
/// only the first chunk of a JSON rowset, so a page ends wherever that chunk
/// does. Pagination continues until a page adds no new names instead of
/// trusting the page size.
const PAGE_LIMIT: usize = 10_000;
/// Stops a source that keeps returning rows we already have from looping forever.
const MAX_PAGES: usize = 1_000;

pub struct ShowSource {
    session: Session,
    account: String,
}

impl ShowSource {
    pub fn new(session: Session, account: &str) -> Self {
        Self {
            session,
            account: account.to_uppercase(),
        }
    }

    fn rows(&self, sql: &str) -> Result<RowSet> {
        RowSet::from_output(self.session.exec(sql)?)
    }

    fn paged(&self, show: &str) -> Result<RowSet> {
        paged(show, |sql| self.rows(sql))
    }
}

fn paged(show: &str, mut page: impl FnMut(&str) -> Result<RowSet>) -> Result<RowSet> {
    let mut all = RowSet::default();
    let mut seen = HashSet::new();
    let mut from: Option<Name> = None;
    for _ in 0..MAX_PAGES {
        let sql = match &from {
            None => format!("{show} LIMIT {PAGE_LIMIT}"),
            Some(last) => format!("{show} LIMIT {PAGE_LIMIT} FROM {}", last.to_sql_string()),
        };
        let rs = page(&sql)?;
        if all.columns.is_empty() {
            all.columns = rs.columns.clone();
        }
        let mut added = false;
        for row in &rs.rows {
            let name = rs.get(row, "name")?;
            from = Some(Name::new(name));
            if seen.insert(name.to_string()) {
                all.rows.push(row.clone());
                added = true;
            }
        }
        if !added {
            return Ok(all);
        }
    }
    bail!("gave up paging `{show}` after {MAX_PAGES} pages")
}

impl MetadataSource for ShowSource {
    fn identify(&mut self) -> Result<(CatalogKey, CurrentScope)> {
        let out = self
            .session
            .exec("SELECT CURRENT_ROLE(), CURRENT_DATABASE(), CURRENT_SCHEMA()")?;
        let QueryOutput::Arrow(batches) = out else {
            bail!("unexpected result for CURRENT_ROLE()");
        };
        let batch = batches
            .first()
            .ok_or_else(|| eyre!("CURRENT_ROLE() returned no rows"))?;
        let cell = |i: usize| -> Option<Name> {
            let col = batch.column(i).as_string_opt::<i32>()?;
            (!col.is_null(0)).then(|| Name::new(col.value(0)))
        };
        let role = cell(0).ok_or_else(|| eyre!("the session has no current role"))?;
        let key = CatalogKey {
            account: self.account.clone(),
            role,
        };
        let current = CurrentScope {
            database: cell(1),
            schema: cell(2),
        };
        Ok((key, current))
    }

    fn fetch(&mut self, scope: &Scope) -> Result<Children> {
        match scope {
            Scope::Account => {
                let rs = self.paged("SHOW DATABASES")?;
                rs.rows
                    .iter()
                    .map(|r| {
                        Ok(Database {
                            name: Name::new(rs.get(r, "name")?),
                            comment: rs.opt(r, "comment"),
                        })
                    })
                    .collect::<Result<_>>()
                    .map(Children::Databases)
            }
            Scope::Database(db) => {
                let rs = self.paged(&format!("SHOW SCHEMAS IN DATABASE {}", db.to_sql()))?;
                rs.rows
                    .iter()
                    .map(|r| {
                        Ok(Schema {
                            name: Name::new(rs.get(r, "name")?),
                            comment: rs.opt(r, "comment"),
                        })
                    })
                    .collect::<Result<_>>()
                    .map(Children::Schemas)
            }
            Scope::Schema(db, schema) => {
                let rs = self.paged(&format!(
                    "SHOW OBJECTS IN SCHEMA {}.{}",
                    db.to_sql(),
                    schema.to_sql()
                ))?;
                rs.rows
                    .iter()
                    .map(|r| relation(&rs, r))
                    .collect::<Result<_>>()
                    .map(Children::Relations)
            }
            Scope::Relation(db, schema, rel) => {
                let rs = self.rows(&format!(
                    "SHOW COLUMNS IN {}.{}.{}",
                    db.to_sql(),
                    schema.to_sql(),
                    rel.to_sql()
                ))?;
                rs.rows
                    .iter()
                    .map(|r| column(&rs, r))
                    .collect::<Result<_>>()
                    .map(Children::Columns)
            }
        }
    }
}

pub struct LocalSource {
    ctx: SessionContext,
    runtime: tokio::runtime::Runtime,
}

impl LocalSource {
    pub fn new(ctx: SessionContext) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        Ok(Self { ctx, runtime })
    }

    fn schema(&self, db: &Name, schema: &Name) -> Result<Arc<dyn SchemaProvider>> {
        self.ctx
            .catalog(db.as_str())
            .and_then(|c| c.schema(schema.as_str()))
            .ok_or_else(|| eyre!("no local schema {}.{}", db.as_str(), schema.as_str()))
    }
}

impl MetadataSource for LocalSource {
    fn identify(&mut self) -> Result<(CatalogKey, CurrentScope)> {
        let options = self.ctx.state().config_options().catalog.clone();
        let key = CatalogKey {
            account: "local".into(),
            role: Name::new("local"),
        };
        let current = CurrentScope {
            database: Some(Name::new(options.default_catalog)),
            schema: Some(Name::new(options.default_schema)),
        };
        Ok((key, current))
    }

    fn fetch(&mut self, scope: &Scope) -> Result<Children> {
        let named = |names: Vec<String>| {
            let mut names: Vec<Name> = names.into_iter().map(Name::new).collect();
            names.sort();
            names
        };
        Ok(match scope {
            Scope::Account => Children::Databases(
                named(self.ctx.catalog_names())
                    .into_iter()
                    .map(|name| Database { name, comment: None })
                    .collect(),
            ),
            Scope::Database(db) => {
                let catalog = self
                    .ctx
                    .catalog(db.as_str())
                    .ok_or_else(|| eyre!("no local catalog {}", db.as_str()))?;
                Children::Schemas(
                    named(catalog.schema_names())
                        .into_iter()
                        .map(|name| Schema { name, comment: None })
                        .collect(),
                )
            }
            Scope::Schema(db, schema) => Children::Relations(
                named(self.schema(db, schema)?.table_names())
                    .into_iter()
                    .map(|name| Relation {
                        name,
                        kind: RelationKind::Table,
                        rows: None,
                        bytes: None,
                        comment: None,
                    })
                    .collect(),
            ),
            Scope::Relation(db, schema, rel) => {
                let provider = self.schema(db, schema)?;
                let table = self
                    .runtime
                    .block_on(provider.table(rel.as_str()))?
                    .ok_or_else(|| eyre!("no local table {}", rel.as_str()))?;
                Children::Columns(
                    table
                        .schema()
                        .fields()
                        .iter()
                        .map(|f| Column {
                            name: Name::new(f.name()),
                            data_type: f.data_type().to_string(),
                            nullable: f.is_nullable(),
                            comment: None,
                        })
                        .collect(),
                )
            }
        })
    }

    fn folding(&self) -> Folding {
        Folding::Lower
    }
}

fn relation(rs: &RowSet, r: &[Option<String>]) -> Result<Relation> {
    let flag = |col: &str| rs.opt(r, col).is_some_and(|v| v.eq_ignore_ascii_case("Y"));
    let kind = if flag("is_dynamic") {
        RelationKind::Dynamic
    } else if flag("is_iceberg") {
        RelationKind::Iceberg
    } else {
        match rs.get(r, "kind")? {
            "TABLE" => RelationKind::Table,
            "VIEW" => RelationKind::View,
            other => RelationKind::Other(other.to_string()),
        }
    };
    let number = |col: &str| rs.opt(r, col).and_then(|v| v.parse().ok());
    Ok(Relation {
        name: Name::new(rs.get(r, "name")?),
        kind,
        rows: number("rows"),
        bytes: number("bytes"),
        comment: rs.opt(r, "comment"),
    })
}

fn column(rs: &RowSet, r: &[Option<String>]) -> Result<Column> {
    let raw = rs.get(r, "data_type")?;
    let ty: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| eyre!("parsing data_type {raw}: {e}"))?;
    Ok(Column {
        name: Name::new(rs.get(r, "column_name")?),
        data_type: ty["type"].as_str().unwrap_or("UNKNOWN").to_string(),
        nullable: ty["nullable"].as_bool().unwrap_or(true),
        comment: rs.opt(r, "comment"),
    })
}

#[derive(Debug, Default)]
struct RowSet {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
}

impl RowSet {
    fn from_output(out: QueryOutput) -> Result<Self> {
        match out {
            QueryOutput::Rows { columns, rows } => Ok(Self { columns, rows }),
            QueryOutput::Empty => Ok(Self::default()),
            QueryOutput::Arrow(_) => bail!("expected a SHOW result but got Arrow data"),
        }
    }

    fn index(&self, col: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|c| c == col)
            .ok_or_else(|| eyre!("SHOW result has no `{col}` column"))
    }

    fn get<'a>(&self, row: &'a [Option<String>], col: &str) -> Result<&'a str> {
        row.get(self.index(col)?)
            .and_then(|v| v.as_deref())
            .ok_or_else(|| eyre!("`{col}` is null"))
    }

    /// Empty strings count as absent: SHOW reports a missing comment as "".
    fn opt(&self, row: &[Option<String>], col: &str) -> Option<String> {
        let i = self.index(col).ok()?;
        row.get(i)?.as_ref().filter(|v| !v.is_empty()).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(columns: &[&str], rows: &[&[&str]]) -> RowSet {
        RowSet {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|v| Some(v.to_string())).collect())
                .collect(),
        }
    }

    #[test]
    fn maps_show_objects_rows_to_relations() {
        let rs = rows(
            &[
                "name",
                "kind",
                "comment",
                "rows",
                "bytes",
                "is_dynamic",
                "is_iceberg",
            ],
            &[
                &["ORDERS", "TABLE", "", "10", "2048", "N", "N"],
                &["V", "VIEW", "a view", "", "", "N", "N"],
                &["D", "TABLE", "", "1", "1", "Y", "N"],
            ],
        );
        let rels: Vec<_> = rs.rows.iter().map(|r| relation(&rs, r).unwrap()).collect();
        assert_eq!(rels[0].kind, RelationKind::Table);
        assert_eq!(rels[0].rows, Some(10));
        assert_eq!(rels[0].comment, None);
        assert_eq!(rels[1].kind, RelationKind::View);
        assert_eq!(rels[1].comment.as_deref(), Some("a view"));
        assert_eq!(rels[2].kind, RelationKind::Dynamic);
    }

    #[test]
    fn maps_show_columns_rows_to_columns() {
        let rs = rows(
            &["column_name", "data_type", "comment"],
            &[&["ID", r#"{"type":"FIXED","nullable":false}"#, "pk"]],
        );
        let c = column(&rs, &rs.rows[0]).unwrap();
        assert_eq!(c.name, Name::new("ID"));
        assert_eq!(c.data_type, "FIXED");
        assert!(!c.nullable);
    }

    #[test]
    fn paging_recovers_rows_lost_to_truncated_pages() {
        let names: Vec<String> = (0..10).map(|i| format!("T{i}")).collect();
        let fetched = paged("SHOW OBJECTS", |sql| {
            let after = sql.split("FROM '").nth(1).map(|s| s.trim_end_matches('\''));
            let page: Vec<&str> = names
                .iter()
                .map(String::as_str)
                .filter(|n| after.is_none_or(|a| *n > a))
                .take(3)
                .collect();
            Ok(rows(
                &["name"],
                &page
                    .iter()
                    .map(std::slice::from_ref)
                    .collect::<Vec<_>>(),
            ))
        })
        .unwrap();
        let got: Vec<_> = fetched.rows.iter().map(|r| r[0].clone().unwrap()).collect();
        assert_eq!(got, names);
    }

    #[test]
    fn paging_ends_when_from_repeats_the_boundary_row() {
        let names = ["A", "B", "C", "D"];
        let fetched = paged("SHOW OBJECTS", |sql| {
            let after = sql.split("FROM '").nth(1).map(|s| s.trim_end_matches('\''));
            let page: Vec<&str> = names
                .iter()
                .copied()
                .filter(|n| after.is_none_or(|a| *n >= a))
                .take(2)
                .collect();
            Ok(rows(&["name"], &page.iter().map(std::slice::from_ref).collect::<Vec<_>>()))
        })
        .unwrap();
        let got: Vec<_> = fetched.rows.iter().map(|r| r[0].clone().unwrap()).collect();
        assert_eq!(got, names);
    }

    #[test]
    fn empty_output_is_an_empty_row_set() {
        let rs = RowSet::from_output(QueryOutput::Empty).unwrap();
        assert!(rs.rows.is_empty());
    }
}
