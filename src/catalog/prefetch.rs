use super::{CurrentScope, Priority, Scope};

#[derive(Debug, Default)]
pub struct Prefetcher;

impl Prefetcher {
    pub fn on_connected(&self, current: &CurrentScope) -> Vec<(Scope, Priority)> {
        match (&current.database, &current.schema) {
            (Some(db), Some(schema)) => {
                vec![(Scope::Schema(db.clone(), schema.clone()), Priority::Context)]
            }
            _ => Vec::new(),
        }
    }
}
