use std::ops::Range;
use std::time::SystemTime;

use frizbee::{CaseMatching, Config, Matcher};

use super::context::{CompletionContext, Expect};
use crate::catalog::{
    Catalog, Children, CurrentScope, Folding, Freshness, Name, RelationKind, Scope,
};

const MAX_CANDIDATES: usize = 50;
/// Ranks names that start with what was typed above looser fuzzy matches.
const PREFIX_BONUS: u32 = 1_000;

/// Declaration order is the tie-break order in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateKind {
    Table,
    View,
    Schema,
    Database,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub insert: String,
    pub kind: CandidateKind,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub candidates: Vec<Candidate>,
    pub replace: Range<usize>,
    /// Scopes that must load before the list is complete.
    pub needs: Vec<Scope>,
    /// Why the scope could not be listed, when it has never loaded.
    pub error: Option<String>,
}

const DESCEND: &str = ".";

struct Item<'a> {
    name: &'a Name,
    kind: CandidateKind,
    detail: Option<String>,
    suffix: &'static str,
}

struct Pool<'a> {
    catalog: &'a Catalog,
    now: SystemTime,
    out: Completion,
    items: Vec<Item<'a>>,
}

impl<'a> Pool<'a> {
    fn view(&mut self, scope: &Scope) -> Option<&'a Children> {
        let view = self.catalog.view(scope, self.now);
        match view.freshness {
            Freshness::Fresh => {}
            Freshness::Failed => self.out.error = view.error.map(str::to_string),
            _ => self.out.needs.push(scope.clone()),
        }
        view.children
    }

    fn add(&mut self, scope: Scope, suffix: &'static str) {
        let Some(children) = self.view(&scope) else {
            return;
        };
        let item = |name, kind, detail| Item {
            name,
            kind,
            detail,
            suffix,
        };
        match children {
            Children::Databases(v) => self.items.extend(
                v.iter()
                    .map(|d| item(&d.name, CandidateKind::Database, d.comment.clone())),
            ),
            Children::Schemas(v) => self.items.extend(
                v.iter()
                    .map(|s| item(&s.name, CandidateKind::Schema, s.comment.clone())),
            ),
            Children::Relations(v) => self.items.extend(v.iter().map(|r| {
                let kind = match r.kind {
                    RelationKind::View => CandidateKind::View,
                    _ => CandidateKind::Table,
                };
                item(&r.name, kind, r.rows.map(|n| format!("{n} rows")))
            })),
            Children::Columns(_) => {}
        }
    }

    /// Adds the children of `parent`'s child `name`, but only once `parent`'s
    /// list contains it: guessing would send a SHOW for a name that may not
    /// exist on every keystroke. Until then `parent` is requested instead, and
    /// its arrival re-runs completion.
    fn add_if_listed(&mut self, parent: Scope, name: &Name, scope: Scope, suffix: &'static str) {
        if self.view(&parent).is_some_and(|c| c.names().contains(&name)) {
            self.add(scope, suffix);
        }
    }
}

pub fn complete(
    ctx: &CompletionContext,
    catalog: &Catalog,
    current: &CurrentScope,
    now: SystemTime,
) -> Completion {
    let mut pool = Pool {
        catalog,
        now,
        out: Completion {
            replace: ctx.replace.clone(),
            ..Default::default()
        },
        items: Vec::new(),
    };
    let db = current.database.as_ref();
    let folding = catalog.folding();
    let names: Vec<Name> = ctx
        .qualifier
        .iter()
        .map(|p| Name::from_ident_with(p, folding))
        .collect();
    match (ctx.expect, names.as_slice()) {
        (Expect::Relation, []) => {
            if let (Some(d), Some(s)) = (db, &current.schema) {
                pool.add(Scope::Schema(d.clone(), s.clone()), "");
            }
            if let Some(d) = db {
                pool.add(Scope::Database(d.clone()), DESCEND);
            }
            pool.add(Scope::Account, DESCEND);
        }
        (Expect::Relation, [x]) => {
            pool.add_if_listed(Scope::Account, x, Scope::Database(x.clone()), DESCEND);
            if let Some(d) = db {
                let schema = Scope::Schema(d.clone(), x.clone());
                pool.add_if_listed(Scope::Database(d.clone()), x, schema, "");
            }
        }
        (Expect::Relation, [d, s]) => pool.add(Scope::Schema(d.clone(), s.clone()), ""),
        (Expect::Database, []) => pool.add(Scope::Account, ""),
        (Expect::Database | Expect::Schema, [d]) => {
            pool.add_if_listed(Scope::Account, d, Scope::Database(d.clone()), "")
        }
        (Expect::Schema, []) => {
            if let Some(d) = db {
                pool.add(Scope::Database(d.clone()), "");
            }
            pool.add(Scope::Account, DESCEND);
        }
        _ => {}
    }
    let Pool { out, items, .. } = pool;
    rank(out, items, &ctx.prefix.text, folding)
}

fn rank(mut out: Completion, items: Vec<Item<'_>>, typed: &str, folding: Folding) -> Completion {
    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    let mut scored: Vec<(u32, usize)> = if typed.is_empty() {
        (0..names.len()).map(|i| (0, i)).collect()
    } else {
        let upper = typed.to_uppercase();
        // Smart casing would make `Ord` miss `ORDERS`; Snowflake names are case-folded anyway.
        let config = Config {
            casing: CaseMatching::Ignore,
            ..Config::default()
        };
        let mut matcher = Matcher::new(typed, &config);
        matcher
            .match_list(&names)
            .into_iter()
            .map(|m| {
                let i = m.index as usize;
                let bonus = if names[i].to_uppercase().starts_with(&upper) {
                    PREFIX_BONUS
                } else {
                    0
                };
                (m.score as u32 + bonus, i)
            })
            .collect()
    };
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| items[a.1].kind.cmp(&items[b.1].kind))
            .then_with(|| names[a.1].cmp(names[b.1]))
    });

    out.candidates = scored
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|(_, i)| {
            let item = &items[i];
            Candidate {
                label: item.name.as_str().to_string(),
                insert: format!("{}{}", item.name.to_sql_with(folding), item.suffix),
                kind: item.kind,
                detail: item.detail.clone(),
            }
        })
        .collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CatalogKey, CatalogPatch, Relation};
    use crate::complete::context::analyze;

    fn catalog_with(scope: &Scope, names: &[&str], now: SystemTime) -> Catalog {
        let mut c = Catalog::new(CatalogKey {
            account: "A".into(),
            role: Name::new("R"),
        });
        let g = c.begin_fetch(scope, now).unwrap();
        c.apply(CatalogPatch {
            scope: scope.clone(),
            generation: g,
            fetched_at: now,
            result: Ok(Children::Relations(
                names
                    .iter()
                    .map(|n| Relation {
                        name: Name::new(*n),
                        kind: RelationKind::Table,
                        rows: None,
                        bytes: None,
                        comment: None,
                    })
                    .collect(),
            )),
        });
        c
    }

    fn current() -> CurrentScope {
        CurrentScope {
            database: Some(Name::new("DB")),
            schema: Some(Name::new("S")),
        }
    }

    fn labels(c: &Completion) -> Vec<&str> {
        c.candidates.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn completes_current_schema_relations_prefix_first() {
        let now = SystemTime::now();
        let scope = Scope::Schema(Name::new("DB"), Name::new("S"));
        let cat = catalog_with(&scope, &["USER_ORDERS", "ORDERS", "CUSTOMERS"], now);
        let ctx = analyze("SELECT * FROM ord", 17).unwrap();
        let c = complete(&ctx, &cat, &current(), now);
        assert_eq!(labels(&c), vec!["ORDERS", "USER_ORDERS"]);
        assert!(!c.needs.contains(&scope));
    }

    #[test]
    fn qualified_schema_is_requested_when_missing() {
        let now = SystemTime::now();
        let cat = catalog_with(&Scope::Schema(Name::new("DB"), Name::new("S")), &[], now);
        let ctx = analyze("SELECT * FROM other.pub.", 24).unwrap();
        let c = complete(&ctx, &cat, &current(), now);
        assert!(c.candidates.is_empty());
        assert_eq!(
            c.needs,
            vec![Scope::Schema(Name::new("OTHER"), Name::new("PUB"))]
        );
    }

    #[test]
    fn a_failed_scope_reports_its_error_instead_of_loading() {
        let now = SystemTime::now();
        let scope = Scope::Schema(Name::new("NO"), Name::new("SUCH"));
        let mut cat = catalog_with(&Scope::Schema(Name::new("DB"), Name::new("S")), &[], now);
        let g = cat.begin_fetch(&scope, now).unwrap();
        cat.apply(CatalogPatch {
            scope,
            generation: g,
            fetched_at: now,
            result: Err("does not exist".into()),
        });
        let ctx = analyze("SELECT * FROM no.such.", 22).unwrap();
        let c = complete(&ctx, &cat, &current(), now);
        assert!(c.needs.is_empty());
        assert_eq!(c.error.as_deref(), Some("does not exist"));
    }

    fn load(c: &mut Catalog, scope: Scope, children: Children, now: SystemTime) {
        let g = c.begin_fetch(&scope, now).unwrap();
        c.apply(CatalogPatch {
            scope,
            generation: g,
            fetched_at: now,
            result: Ok(children),
        });
    }

    fn dbs(names: &[&str]) -> Children {
        Children::Databases(
            names
                .iter()
                .map(|n| crate::catalog::Database {
                    name: Name::new(*n),
                    comment: None,
                })
                .collect(),
        )
    }

    fn schemas(names: &[&str]) -> Children {
        Children::Schemas(
            names
                .iter()
                .map(|n| crate::catalog::Schema {
                    name: Name::new(*n),
                    comment: None,
                })
                .collect(),
        )
    }

    fn inserts(c: &Completion) -> Vec<&str> {
        c.candidates.iter().map(|c| c.insert.as_str()).collect()
    }

    #[test]
    fn unqualified_relation_offers_tables_then_schemas_then_databases() {
        let now = SystemTime::now();
        let mut cat = catalog_with(&Scope::Schema(Name::new("DB"), Name::new("S")), &["SALES"], now);
        load(&mut cat, Scope::Database(Name::new("DB")), schemas(&["S", "STAGING"]), now);
        load(&mut cat, Scope::Account, dbs(&["DB", "SANDBOX"]), now);
        let ctx = analyze("SELECT * FROM s", 15).unwrap();
        let c = complete(&ctx, &cat, &current(), now);
        assert_eq!(inserts(&c), vec!["SALES", "S.", "STAGING.", "SANDBOX."]);
        assert!(c.needs.is_empty());
    }

    #[test]
    fn one_qualifier_is_a_database_or_a_schema_of_the_current_database() {
        let now = SystemTime::now();
        let mut cat = catalog_with(&Scope::Schema(Name::new("DB"), Name::new("RAW")), &["EVENTS"], now);
        load(&mut cat, Scope::Account, dbs(&["DB", "OTHER"]), now);
        load(&mut cat, Scope::Database(Name::new("DB")), schemas(&["RAW"]), now);
        load(&mut cat, Scope::Database(Name::new("OTHER")), schemas(&["PUBLIC"]), now);

        let ctx = analyze("SELECT * FROM other.", 20).unwrap();
        assert_eq!(inserts(&complete(&ctx, &cat, &current(), now)), vec!["PUBLIC."]);

        let ctx = analyze("SELECT * FROM raw.", 18).unwrap();
        assert_eq!(inserts(&complete(&ctx, &cat, &current(), now)), vec!["EVENTS"]);
    }

    #[test]
    fn an_unknown_qualifier_is_not_fetched_once_the_parent_is_listed() {
        let now = SystemTime::now();
        let mut cat = catalog_with(&Scope::Schema(Name::new("DB"), Name::new("S")), &[], now);
        load(&mut cat, Scope::Account, dbs(&["DB"]), now);
        load(&mut cat, Scope::Database(Name::new("DB")), schemas(&["S"]), now);
        let ctx = analyze("SELECT * FROM nope.", 19).unwrap();
        let c = complete(&ctx, &cat, &current(), now);
        assert!(c.candidates.is_empty());
        assert!(c.needs.is_empty());
    }

    #[test]
    fn an_unlisted_parent_is_requested_before_its_child() {
        let now = SystemTime::now();
        let cat = catalog_with(&Scope::Schema(Name::new("DB"), Name::new("S")), &[], now);
        let ctx = analyze("SELECT * FROM other.", 20).unwrap();
        let c = complete(&ctx, &cat, &current(), now);
        assert_eq!(c.needs, vec![Scope::Account, Scope::Database(Name::new("DB"))]);
    }

    #[test]
    fn a_stale_parent_without_the_name_does_not_trigger_a_guess() {
        let now = SystemTime::now();
        let mut cat = catalog_with(&Scope::Schema(Name::new("DB"), Name::new("S")), &[], now);
        load(&mut cat, Scope::Account, dbs(&["DB"]), now);
        load(&mut cat, Scope::Database(Name::new("DB")), schemas(&["S"]), now);
        let later = now + std::time::Duration::from_secs(2 * 60 * 60);
        let ctx = analyze("SELECT * FROM nope.", 19).unwrap();
        let c = complete(&ctx, &cat, &current(), later);
        assert_eq!(c.needs, vec![Scope::Account, Scope::Database(Name::new("DB"))]);
    }

    #[test]
    fn use_statements_complete_databases_and_schemas() {
        let now = SystemTime::now();
        let mut cat = catalog_with(&Scope::Schema(Name::new("DB"), Name::new("S")), &[], now);
        load(&mut cat, Scope::Account, dbs(&["DB"]), now);
        load(&mut cat, Scope::Database(Name::new("DB")), schemas(&["S"]), now);

        let ctx = analyze("USE DATABASE ", 13).unwrap();
        assert_eq!(inserts(&complete(&ctx, &cat, &current(), now)), vec!["DB"]);
        let ctx = analyze("USE SCHEMA ", 11).unwrap();
        assert_eq!(inserts(&complete(&ctx, &cat, &current(), now)), vec!["S", "DB."]);
        let ctx = analyze("USE db.", 7).unwrap();
        assert_eq!(inserts(&complete(&ctx, &cat, &current(), now)), vec!["S"]);
    }

    #[test]
    fn inserts_quoted_names_when_needed() {
        let now = SystemTime::now();
        let scope = Scope::Schema(Name::new("DB"), Name::new("S"));
        let cat = catalog_with(&scope, &["mixedCase"], now);
        let ctx = analyze("SELECT * FROM ", 14).unwrap();
        let c = complete(&ctx, &cat, &current(), now);
        assert_eq!(c.candidates[0].insert, "\"mixedCase\"");
    }
}
