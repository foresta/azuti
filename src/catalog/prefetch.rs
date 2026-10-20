use super::{CurrentScope, Priority, Scope};

#[derive(Debug, Default)]
pub struct Prefetcher;

impl Prefetcher {
    pub fn on_connected(&self, current: &CurrentScope) -> Vec<(Scope, Priority)> {
        let mut scopes = vec![Scope::Account];
        if let Some(db) = &current.database {
            scopes.push(Scope::Database(db.clone()));
            if let Some(schema) = &current.schema {
                scopes.push(Scope::Schema(db.clone(), schema.clone()));
            }
        }
        scopes.into_iter().map(|s| (s, Priority::Context)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Name;

    fn scopes(current: CurrentScope) -> Vec<Scope> {
        Prefetcher.on_connected(&current).into_iter().map(|(s, _)| s).collect()
    }

    #[test]
    fn prefetches_down_to_the_deepest_current_level() {
        let db = Name::new("DB");
        let s = Name::new("S");
        assert_eq!(scopes(CurrentScope::default()), vec![Scope::Account]);
        let only_db = CurrentScope {
            database: Some(db.clone()),
            schema: None,
        };
        assert_eq!(scopes(only_db), vec![Scope::Account, Scope::Database(db.clone())]);
        let both = CurrentScope {
            database: Some(db.clone()),
            schema: Some(s.clone()),
        };
        assert_eq!(
            scopes(both),
            vec![Scope::Account, Scope::Database(db.clone()), Scope::Schema(db, s)]
        );
    }
}
