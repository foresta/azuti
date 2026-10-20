pub mod prefetch;
pub mod source;
pub mod worker;

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// A name as Snowflake stores it: unquoted identifiers are upper-cased.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentPart {
    pub text: String,
    pub quoted: bool,
}

/// Words that cannot be used as a bare identifier, so names equal to them are
/// always quoted on insertion.
const RESERVED: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CHECK",
    "COLUMN",
    "CONNECT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "DELETE",
    "DISTINCT",
    "DROP",
    "ELSE",
    "EXISTS",
    "FALSE",
    "FOLLOWING",
    "FOR",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INCREMENT",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "MINUS",
    "NATURAL",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "QUALIFY",
    "REGEXP",
    "REVOKE",
    "RIGHT",
    "RLIKE",
    "ROW",
    "ROWS",
    "SAMPLE",
    "SELECT",
    "SET",
    "SOME",
    "START",
    "TABLE",
    "TABLESAMPLE",
    "THEN",
    "TO",
    "TRIGGER",
    "TRUE",
    "TRY_CAST",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHENEVER",
    "WHERE",
    "WINDOW",
    "WITH",
];

impl Name {
    pub fn new(s: impl Into<String>) -> Self {
        Name(s.into())
    }

    pub fn from_ident(part: &IdentPart) -> Self {
        if part.quoted {
            Name(part.text.clone())
        } else {
            Name(part.text.to_uppercase())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_sql(&self) -> String {
        let s = &self.0;
        let bare = s
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            && s.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '$')
            && !RESERVED.contains(&s.as_str());
        if bare {
            s.clone()
        } else {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
    }

    pub fn to_sql_string(&self) -> String {
        format!("'{}'", self.0.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogKey {
    pub account: String,
    pub role: Name,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    Account,
    Database(Name),
    Schema(Name, Name),
    Relation(Name, Name, Name),
}

impl Scope {
    fn path(&self) -> Vec<&Name> {
        match self {
            Scope::Account => vec![],
            Scope::Database(d) => vec![d],
            Scope::Schema(d, s) => vec![d, s],
            Scope::Relation(d, s, r) => vec![d, s, r],
        }
    }

    /// The scope holding the children of `child`, a direct child of this scope.
    fn child(&self, child: &Name) -> Option<Scope> {
        let c = child.clone();
        match self {
            Scope::Account => Some(Scope::Database(c)),
            Scope::Database(d) => Some(Scope::Schema(d.clone(), c)),
            Scope::Schema(d, s) => Some(Scope::Relation(d.clone(), s.clone(), c)),
            Scope::Relation(..) => None,
        }
    }

    fn is_within(&self, ancestor: &Scope) -> bool {
        let (me, anc) = (self.path(), ancestor.path());
        me.len() >= anc.len() && me[..anc.len()] == anc[..]
    }

    fn ttl(&self) -> Duration {
        match self {
            Scope::Account | Scope::Database(_) => Duration::from_secs(60 * 60),
            Scope::Schema(..) | Scope::Relation(..) => Duration::from_secs(30 * 60),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurrentScope {
    pub database: Option<Name>,
    pub schema: Option<Name>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub name: Name,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub name: Name,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub name: Name,
    pub kind: RelationKind,
    pub rows: Option<u64>,
    pub bytes: Option<u64>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
    Dynamic,
    Iceberg,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: Name,
    pub data_type: String,
    pub nullable: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Children {
    Databases(Vec<Database>),
    Schemas(Vec<Schema>),
    Relations(Vec<Relation>),
    Columns(Vec<Column>),
}

impl Children {
    pub fn names(&self) -> Vec<&Name> {
        match self {
            Children::Databases(v) => v.iter().map(|x| &x.name).collect(),
            Children::Schemas(v) => v.iter().map(|x| &x.name).collect(),
            Children::Relations(v) => v.iter().map(|x| &x.name).collect(),
            Children::Columns(v) => v.iter().map(|x| &x.name).collect(),
        }
    }

    fn fits(&self, scope: &Scope) -> bool {
        matches!(
            (scope, self),
            (Scope::Account, Children::Databases(_))
                | (Scope::Database(_), Children::Schemas(_))
                | (Scope::Schema(..), Children::Relations(_))
                | (Scope::Relation(..), Children::Columns(_))
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Demand = 0,
    Context = 1,
    Crawl = 2,
}

#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub scope: Scope,
    pub generation: u64,
    pub priority: Priority,
}

#[derive(Debug)]
pub struct CatalogPatch {
    pub scope: Scope,
    pub generation: u64,
    pub fetched_at: SystemTime,
    pub result: Result<Children, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Missing,
    Loading,
    Fresh,
    Stale,
    Failed,
}

#[derive(Debug)]
pub struct ScopeView<'a> {
    pub freshness: Freshness,
    pub children: Option<&'a Children>,
    pub error: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Discarded,
}

/// A failed scope is not refetched sooner than this, so a name that does not
/// exist is not re-queried on every keystroke.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct Entry {
    children: Option<Children>,
    fetched_at: Option<SystemTime>,
    in_flight: Option<u64>,
    invalidated: bool,
    error: Option<(String, SystemTime)>,
}

#[derive(Debug)]
pub struct Catalog {
    #[allow(dead_code)] // TODO: read when CacheStore persists per (account, role)
    key: CatalogKey,
    entries: HashMap<Scope, Entry>,
    /// Tickets are unique across the whole catalog, not per entry: an entry
    /// dropped and recreated must never accept a patch issued before the drop.
    next_ticket: u64,
}

impl Catalog {
    pub fn new(key: CatalogKey) -> Self {
        Self {
            key,
            entries: HashMap::new(),
            next_ticket: 0,
        }
    }

    pub fn view(&self, scope: &Scope, now: SystemTime) -> ScopeView<'_> {
        let Some(e) = self.entries.get(scope) else {
            return ScopeView {
                freshness: Freshness::Missing,
                children: None,
                error: None,
            };
        };
        let freshness = match (&e.children, &e.error, e.in_flight) {
            (_, _, Some(_)) => Freshness::Loading,
            (None, Some(_), None) => Freshness::Failed,
            (None, None, None) => Freshness::Missing,
            (Some(_), err, None) => {
                let expired = e
                    .fetched_at
                    .is_none_or(|at| now.duration_since(at).unwrap_or_default() > scope.ttl());
                if e.invalidated || expired || err.is_some() {
                    Freshness::Stale
                } else {
                    Freshness::Fresh
                }
            }
        };
        ScopeView {
            freshness,
            children: e.children.as_ref(),
            error: e.error.as_ref().map(|(msg, _)| msg.as_str()),
        }
    }

    pub fn begin_fetch(&mut self, scope: &Scope, now: SystemTime) -> Option<u64> {
        let e = self.entries.entry(scope.clone()).or_default();
        if e.in_flight.is_some() {
            return None;
        }
        if let Some((_, at)) = &e.error {
            if now.duration_since(*at).unwrap_or_default() < FAILURE_BACKOFF {
                return None;
            }
        }
        self.next_ticket += 1;
        e.in_flight = Some(self.next_ticket);
        Some(self.next_ticket)
    }

    pub fn apply(&mut self, patch: CatalogPatch) -> ApplyOutcome {
        let Some(e) = self.entries.get_mut(&patch.scope) else {
            return ApplyOutcome::Discarded;
        };
        if e.in_flight != Some(patch.generation) {
            return ApplyOutcome::Discarded;
        }
        e.in_flight = None;
        match patch.result {
            Ok(children) if children.fits(&patch.scope) => {
                let removed: Vec<Name> = match &e.children {
                    Some(old) => {
                        let keep = children.names();
                        old.names()
                            .into_iter()
                            .filter(|n| !keep.contains(n))
                            .cloned()
                            .collect()
                    }
                    None => Vec::new(),
                };
                e.children = Some(children);
                e.fetched_at = Some(patch.fetched_at);
                e.invalidated = false;
                e.error = None;
                for name in removed {
                    if let Some(gone) = patch.scope.child(&name) {
                        self.entries.retain(|s, _| !s.is_within(&gone));
                    }
                }
            }
            Ok(_) => e.error = Some(("result does not fit the scope".into(), patch.fetched_at)),
            Err(err) => e.error = Some((err, patch.fetched_at)),
        }
        ApplyOutcome::Applied
    }

    #[allow(dead_code)] // TODO: called from statement_effects (DDL / USE detection)
    pub fn invalidate(&mut self, scope: &Scope, recursive: bool) {
        for (s, e) in self.entries.iter_mut() {
            if s == scope || (recursive && s.is_within(scope)) {
                e.in_flight = None;
                e.invalidated = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> CatalogKey {
        CatalogKey {
            account: "ACC".into(),
            role: Name::new("R"),
        }
    }

    fn schema() -> Scope {
        Scope::Schema(Name::new("DB"), Name::new("S"))
    }

    fn rel(name: &str) -> Relation {
        Relation {
            name: Name::new(name),
            kind: RelationKind::Table,
            rows: None,
            bytes: None,
            comment: None,
        }
    }

    fn patch(scope: Scope, generation: u64, at: SystemTime, names: &[&str]) -> CatalogPatch {
        CatalogPatch {
            scope,
            generation,
            fetched_at: at,
            result: Ok(Children::Relations(names.iter().map(|n| rel(n)).collect())),
        }
    }

    #[test]
    fn to_sql_quotes_only_when_needed() {
        assert_eq!(Name::new("ORDERS").to_sql(), "ORDERS");
        assert_eq!(Name::new("orders").to_sql(), "\"orders\"");
        assert_eq!(Name::new("ORDER").to_sql(), "\"ORDER\"");
        assert_eq!(Name::new("A\"B").to_sql(), "\"A\"\"B\"");
        assert_eq!(Name::new("it's").to_sql_string(), "'it\\'s'");
    }

    #[test]
    fn fetch_lifecycle_goes_missing_loading_fresh_stale() {
        let now = SystemTime::now();
        let mut c = Catalog::new(key());
        assert_eq!(c.view(&schema(), now).freshness, Freshness::Missing);
        let g = c.begin_fetch(&schema(), now).unwrap();
        assert_eq!(c.view(&schema(), now).freshness, Freshness::Loading);
        assert_eq!(c.begin_fetch(&schema(), now), None);
        assert_eq!(
            c.apply(patch(schema(), g, now, &["T"])),
            ApplyOutcome::Applied
        );
        assert_eq!(c.view(&schema(), now).freshness, Freshness::Fresh);
        let later = now + Duration::from_secs(31 * 60);
        assert_eq!(c.view(&schema(), later).freshness, Freshness::Stale);
    }

    #[test]
    fn invalidate_discards_in_flight_result_and_allows_refetch() {
        let now = SystemTime::now();
        let mut c = Catalog::new(key());
        let old = c.begin_fetch(&schema(), now).unwrap();
        c.invalidate(&schema(), false);
        let new = c.begin_fetch(&schema(), now).unwrap();
        assert_ne!(old, new);
        assert_eq!(
            c.apply(patch(schema(), old, now, &["OLD"])),
            ApplyOutcome::Discarded
        );
        assert_eq!(c.view(&schema(), now).freshness, Freshness::Loading);
        assert_eq!(
            c.apply(patch(schema(), new, now, &["NEW"])),
            ApplyOutcome::Applied
        );
        assert_eq!(c.view(&schema(), now).freshness, Freshness::Fresh);
    }

    #[test]
    fn failure_keeps_children_and_backs_off() {
        let now = SystemTime::now();
        let mut c = Catalog::new(key());
        let g = c.begin_fetch(&schema(), now).unwrap();
        c.apply(patch(schema(), g, now, &["T"]));
        c.invalidate(&schema(), false);
        let g = c.begin_fetch(&schema(), now).unwrap();
        c.apply(CatalogPatch {
            scope: schema(),
            generation: g,
            fetched_at: now,
            result: Err("boom".into()),
        });
        let v = c.view(&schema(), now);
        assert_eq!(v.freshness, Freshness::Stale);
        assert!(v.children.is_some());
        assert_eq!(c.begin_fetch(&schema(), now + Duration::from_secs(5)), None);
        assert!(c
            .begin_fetch(&schema(), now + Duration::from_secs(31))
            .is_some());
    }

    #[test]
    fn a_recreated_entry_rejects_patches_issued_before_it_was_dropped() {
        let now = SystemTime::now();
        let mut c = Catalog::new(key());
        let g = c.begin_fetch(&schema(), now).unwrap();
        c.apply(patch(schema(), g, now, &["A"]));
        let col_scope = Scope::Relation(Name::new("DB"), Name::new("S"), Name::new("A"));
        let stale_ticket = c.begin_fetch(&col_scope, now).unwrap();
        c.invalidate(&schema(), false);
        let g = c.begin_fetch(&schema(), now).unwrap();
        c.apply(patch(schema(), g, now, &[]));
        c.begin_fetch(&col_scope, now).unwrap();
        let late = CatalogPatch {
            scope: col_scope.clone(),
            generation: stale_ticket,
            fetched_at: now,
            result: Ok(Children::Columns(vec![])),
        };
        assert_eq!(c.apply(late), ApplyOutcome::Discarded);
    }

    #[test]
    fn removed_children_drop_their_descendant_scopes() {
        let now = SystemTime::now();
        let mut c = Catalog::new(key());
        let g = c.begin_fetch(&schema(), now).unwrap();
        c.apply(patch(schema(), g, now, &["A", "B"]));
        let col_scope = Scope::Relation(Name::new("DB"), Name::new("S"), Name::new("A"));
        c.begin_fetch(&col_scope, now);
        c.invalidate(&schema(), false);
        let g = c.begin_fetch(&schema(), now).unwrap();
        c.apply(patch(schema(), g, now, &["B"]));
        assert_eq!(c.view(&col_scope, now).freshness, Freshness::Missing);
    }
}
