use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;

use crate::catalog::source::LocalSource;
use crate::catalog::worker::MetaWorker;
use crate::local::{LocalEngine, ResultStore};
use crate::meta::MetaSystem;
use crate::query::SnowflakeEngine;
use crate::snowflake::SnowflakeParams;

/// The result of a submitted query: Arrow batches, or an error message.
pub type QueryOutcome = Result<Vec<arrow::array::RecordBatch>, String>;

pub trait Engine: Debug {
    fn submit(&self, sql: String);
    /// Non-blocking: one finished result, if any.
    fn poll(&self) -> Option<QueryOutcome>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(usize);

impl ConnectionId {
    pub const LOCAL: ConnectionId = ConnectionId(0);
}

#[derive(Debug, Clone)]
pub struct ConnectionSpec {
    pub name: String,
    pub kind: SourceKind,
}

#[derive(Debug, Clone)]
pub enum SourceKind {
    Local,
    Snowflake(SnowflakeParams),
    #[cfg(test)]
    Fake,
}

impl ConnectionSpec {
    pub fn local() -> Self {
        Self {
            name: "local".to_string(),
            kind: SourceKind::Local,
        }
    }

    pub fn snowflake(params: SnowflakeParams) -> Self {
        Self {
            name: "Snowflake".to_string(),
            kind: SourceKind::Snowflake(params),
        }
    }

    fn open(&self, store: &ResultStore) -> Connection {
        let (engine, meta): (Box<dyn Engine>, Option<MetaSystem>) = match &self.kind {
            SourceKind::Local => (
                Box::new(LocalEngine::spawn(store.context())),
                // Without a catalog, local SQL still runs; only completion is lost.
                LocalSource::new(store.context())
                    .ok()
                    .map(|source| MetaSystem::new(MetaWorker::spawn_with(Box::new(source)))),
            ),
            SourceKind::Snowflake(params) => (
                Box::new(SnowflakeEngine::spawn(params.clone())),
                Some(MetaSystem::new(MetaWorker::spawn(params.clone()))),
            ),
            #[cfg(test)]
            SourceKind::Fake => fake::open(),
        };
        Connection { engine, meta }
    }
}

#[derive(Debug)]
pub struct Connection {
    pub engine: Box<dyn Engine>,
    pub meta: Option<MetaSystem>,
}

/// Every configured connection, and the ones opened so far. A connection is
/// opened on first use and then kept, so switching back is instant.
#[derive(Debug)]
pub struct Connections {
    specs: Vec<ConnectionSpec>,
    /// Ordered by id so results arriving in the same tick are applied, and
    /// named `result_N`, in a stable order.
    open: BTreeMap<ConnectionId, Connection>,
    /// Editor text of connections not shown. Kept apart from `open` so that
    /// leaving a connection never has to open it.
    parked_sql: HashMap<ConnectionId, Vec<String>>,
    current: ConnectionId,
    previous: Option<ConnectionId>,
}

impl Default for Connections {
    fn default() -> Self {
        Self {
            specs: vec![ConnectionSpec::local()],
            open: BTreeMap::new(),
            parked_sql: HashMap::new(),
            current: ConnectionId::LOCAL,
            previous: None,
        }
    }
}

impl Connections {
    pub fn add(&mut self, spec: ConnectionSpec) -> ConnectionId {
        self.specs.push(spec);
        ConnectionId(self.specs.len() - 1)
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn current_id(&self) -> ConnectionId {
        self.current
    }

    pub fn current_spec(&self) -> &ConnectionSpec {
        &self.specs[self.current.0]
    }

    pub fn previous(&self) -> Option<ConnectionId> {
        self.previous
    }

    /// Opens the current connection if it is not open yet.
    pub fn current(&mut self, store: &ResultStore) -> &mut Connection {
        let id = self.current;
        let spec = &self.specs[id.0];
        self.open.entry(id).or_insert_with(|| spec.open(store))
    }

    pub fn get_open_mut(&mut self, id: ConnectionId) -> Option<&mut Connection> {
        self.open.get_mut(&id)
    }

    pub fn open_mut(&mut self) -> impl Iterator<Item = (ConnectionId, &mut Connection)> {
        self.open.iter_mut().map(|(id, conn)| (*id, conn))
    }

    /// Makes `id` current, parking `shown` (the editor text) on the connection
    /// left and returning the text parked on `id`. Switching to the current
    /// connection changes nothing and hands `shown` back.
    pub fn switch(&mut self, id: ConnectionId, shown: Vec<String>) -> Vec<String> {
        if id == self.current {
            return shown;
        }
        self.parked_sql.insert(self.current, shown);
        self.previous = Some(self.current);
        self.current = id;
        self.parked_sql.remove(&id).unwrap_or_default()
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use crate::catalog::source::MetadataSource;
    use crate::catalog::{CatalogKey, Children, CurrentScope, Name, Scope};
    use arrow::array::{ArrayRef, Int32Array, RecordBatch};
    use color_eyre::Result;
    use std::sync::{Arc, Mutex};

    /// Stands in for a remote connection, which tests cannot reach: every
    /// query returns one row with one column, `n`.
    #[derive(Debug, Default)]
    pub struct FakeEngine {
        done: Mutex<Vec<QueryOutcome>>,
    }

    impl Engine for FakeEngine {
        fn submit(&self, _sql: String) {
            let col: ArrayRef = Arc::new(Int32Array::from(vec![1]));
            let batch = RecordBatch::try_from_iter([("n", col)]).map_err(|e| e.to_string());
            self.done.lock().unwrap().push(batch.map(|b| vec![b]));
        }

        fn poll(&self) -> Option<QueryOutcome> {
            self.done.lock().unwrap().pop()
        }
    }

    struct EmptySource;

    impl MetadataSource for EmptySource {
        fn identify(&mut self) -> Result<(CatalogKey, CurrentScope)> {
            let key = CatalogKey {
                account: "fake".into(),
                role: Name::new("FAKE"),
            };
            Ok((key, CurrentScope::default()))
        }

        fn fetch(&mut self, _scope: &Scope) -> Result<Children> {
            Ok(Children::Databases(Vec::new()))
        }
    }

    pub(super) fn open() -> (Box<dyn Engine>, Option<MetaSystem>) {
        let meta = MetaSystem::new(MetaWorker::spawn_with(Box::new(EmptySource)));
        (Box::new(FakeEngine::default()), Some(meta))
    }

    pub fn spec() -> ConnectionSpec {
        ConnectionSpec {
            name: "fake".to_string(),
            kind: SourceKind::Fake,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(conns: &mut Connections) -> Vec<ConnectionId> {
        conns.open_mut().map(|(id, _)| id).collect()
    }

    #[test]
    fn switching_parks_editor_text_without_opening_anything() {
        let mut conns = Connections::default();
        let remote = conns.add(fake::spec());

        assert!(conns.switch(remote, vec!["local sql".into()]).is_empty());
        assert_eq!(conns.previous(), Some(ConnectionId::LOCAL));
        assert!(ids(&mut conns).is_empty());

        assert_eq!(conns.switch(ConnectionId::LOCAL, vec!["remote sql".into()]), ["local sql"]);
        assert_eq!(conns.switch(remote, vec![]), ["remote sql"]);
    }

    #[test]
    fn switching_to_the_current_connection_changes_nothing() {
        let mut conns = Connections::default();
        let remote = conns.add(fake::spec());
        conns.switch(remote, vec![]);
        assert_eq!(conns.switch(remote, vec!["kept".into()]), ["kept"]);
        assert_eq!(conns.previous(), Some(ConnectionId::LOCAL));
    }

    #[test]
    fn the_current_connection_opens_on_first_use() {
        let store = ResultStore::default();
        let mut conns = Connections::default();
        let remote = conns.add(fake::spec());
        conns.switch(remote, vec![]);
        conns.current(&store);
        assert_eq!(ids(&mut conns), vec![remote]);
    }
}
