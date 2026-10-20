use std::ops::Range;
use std::time::SystemTime;

use frizbee::{CaseMatching, Config, Matcher};

use super::context::{CompletionContext, Expect};
use crate::catalog::{Catalog, Children, CurrentScope, Freshness, Name, RelationKind, Scope};

const MAX_CANDIDATES: usize = 50;
/// Ranks names that start with what was typed above looser fuzzy matches.
const PREFIX_BONUS: u32 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Table,
    View,
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
    /// Scopes the candidates come from that are not fresh yet.
    pub needs: Vec<Scope>,
    /// Why the scope could not be listed, when it has never loaded.
    pub error: Option<String>,
}

pub fn complete(
    ctx: &CompletionContext,
    catalog: &Catalog,
    current: &CurrentScope,
    now: SystemTime,
) -> Completion {
    let mut out = Completion {
        replace: ctx.replace.clone(),
        ..Default::default()
    };
    if ctx.expect != Expect::Relation {
        return out;
    }
    let scope = match ctx.qualifier.as_slice() {
        [] => match (&current.database, &current.schema) {
            (Some(db), Some(schema)) => Scope::Schema(db.clone(), schema.clone()),
            _ => return out,
        },
        [db, schema] => Scope::Schema(Name::from_ident(db), Name::from_ident(schema)),
        _ => return out,
    };

    let view = catalog.view(&scope, now);
    match view.freshness {
        Freshness::Fresh => {}
        Freshness::Failed => out.error = view.error.map(str::to_string),
        _ => out.needs.push(scope.clone()),
    }
    let Some(Children::Relations(relations)) = view.children else {
        return out;
    };

    let names: Vec<&str> = relations.iter().map(|r| r.name.as_str()).collect();
    let typed = ctx.prefix.text.as_str();
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
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| names[a.1].cmp(names[b.1])));

    out.candidates = scored
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|(_, i)| {
            let r = &relations[i];
            let kind = match r.kind {
                RelationKind::View => CandidateKind::View,
                _ => CandidateKind::Table,
            };
            Candidate {
                label: r.name.as_str().to_string(),
                insert: r.name.to_sql(),
                kind,
                detail: r.rows.map(|n| format!("{n} rows")),
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
        assert!(c.needs.is_empty());
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
