use std::time::SystemTime;

use crate::catalog::prefetch::Prefetcher;
use crate::catalog::worker::{MetaWorker, WorkerEvent};
use crate::catalog::{ApplyOutcome, Catalog, CurrentScope, FetchRequest, Priority, Scope};
use crate::complete::context::CompletionContext;
use crate::complete::rank::{self, Completion};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaStatus {
    Connecting,
    Ready,
    Failed(String),
}

#[derive(Debug)]
pub struct MetaSystem {
    worker: MetaWorker,
    catalog: Option<Catalog>,
    current: CurrentScope,
    prefetcher: Prefetcher,
    pub status: MetaStatus,
}

impl MetaSystem {
    pub fn new(worker: MetaWorker) -> Self {
        Self {
            worker,
            catalog: None,
            current: CurrentScope::default(),
            prefetcher: Prefetcher,
            status: MetaStatus::Connecting,
        }
    }

    /// Returns true when anything completion depends on changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for event in self.worker.poll() {
            match event {
                WorkerEvent::Connected { key, current } => {
                    self.catalog = Some(Catalog::new(key));
                    self.current = current;
                    self.status = MetaStatus::Ready;
                    changed = true;
                    for (scope, priority) in self.prefetcher.on_connected(&self.current) {
                        self.request(scope, priority);
                    }
                }
                WorkerEvent::ConnectFailed(err) => {
                    self.status = MetaStatus::Failed(err);
                    changed = true;
                }
                WorkerEvent::Patch(patch) => {
                    if let Some(catalog) = &mut self.catalog {
                        changed |= catalog.apply(patch) == ApplyOutcome::Applied;
                    }
                }
            }
        }
        changed
    }

    pub fn complete(&mut self, ctx: &CompletionContext) -> Completion {
        let Some(catalog) = &self.catalog else {
            return Completion {
                replace: ctx.replace.clone(),
                ..Default::default()
            };
        };
        let completion = rank::complete(ctx, catalog, &self.current, SystemTime::now());
        for scope in completion.needs.clone() {
            self.request(scope, Priority::Demand);
        }
        completion
    }

    pub fn is_connecting(&self) -> bool {
        self.status == MetaStatus::Connecting
    }

    fn request(&mut self, scope: Scope, priority: Priority) {
        let Some(catalog) = &mut self.catalog else {
            return;
        };
        if let Some(generation) = catalog.begin_fetch(&scope, SystemTime::now()) {
            self.worker.request(FetchRequest {
                scope,
                generation,
                priority,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::source::MetadataSource;
    use crate::catalog::{CatalogKey, Children, Name, Relation, RelationKind};
    use crate::complete::context::analyze;
    use color_eyre::Result;

    struct OneSchema;

    impl MetadataSource for OneSchema {
        fn identify(&mut self) -> Result<(CatalogKey, CurrentScope)> {
            let key = CatalogKey {
                account: "A".into(),
                role: Name::new("R"),
            };
            let current = CurrentScope {
                database: Some(Name::new("DB")),
                schema: Some(Name::new("S")),
            };
            Ok((key, current))
        }

        fn fetch(&mut self, _scope: &Scope) -> Result<Children> {
            Ok(Children::Relations(vec![Relation {
                name: Name::new("ORDERS"),
                kind: RelationKind::Table,
                rows: None,
                bytes: None,
                comment: None,
            }]))
        }
    }

    #[test]
    fn current_schema_is_prefetched_and_completed_after_connecting() {
        let mut meta = MetaSystem::new(MetaWorker::spawn_with(Box::new(OneSchema)));
        let ctx = analyze("SELECT * FROM o", 15).unwrap();
        for _ in 0..500 {
            meta.poll();
            let c = meta.complete(&ctx);
            if !c.candidates.is_empty() {
                assert_eq!(c.candidates[0].label, "ORDERS");
                assert_eq!(meta.status, MetaStatus::Ready);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("no candidates after connecting");
    }
}
