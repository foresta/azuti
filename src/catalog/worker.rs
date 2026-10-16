use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use super::source::{MetadataSource, ShowSource};
use super::{CatalogKey, CatalogPatch, CurrentScope, FetchRequest, Priority, Scope};
use crate::snowflake::{Session, SnowflakeParams};

#[derive(Debug)]
pub enum WorkerEvent {
    Connected {
        key: CatalogKey,
        current: CurrentScope,
    },
    ConnectFailed(String),
    Patch(CatalogPatch),
}

/// Crawling is background work; spacing it out keeps cloud-services load low.
const CRAWL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub struct MetaWorker {
    req_tx: Sender<FetchRequest>,
    event_rx: Receiver<WorkerEvent>,
    _handle: JoinHandle<()>,
}

impl MetaWorker {
    pub fn spawn(params: SnowflakeParams) -> Self {
        Self::start(move |events| match Session::connect(&params) {
            Ok(session) => Some(Box::new(ShowSource::new(session, &params.account)) as _),
            Err(e) => {
                let _ = events.send(WorkerEvent::ConnectFailed(format!("{e:#}")));
                None
            }
        })
    }

    #[cfg(test)]
    pub fn spawn_with(source: Box<dyn MetadataSource + Send>) -> Self {
        Self::start(move |_| Some(source))
    }

    fn start(
        make: impl FnOnce(&Sender<WorkerEvent>) -> Option<Box<dyn MetadataSource + Send>>
            + Send
            + 'static,
    ) -> Self {
        let (req_tx, req_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            if let Some(source) = make(&event_tx) {
                run(source, req_rx, event_tx);
            }
        });
        Self {
            req_tx,
            event_rx,
            _handle: handle,
        }
    }

    pub fn request(&self, req: FetchRequest) {
        let _ = self.req_tx.send(req);
    }

    pub fn poll(&self) -> Vec<WorkerEvent> {
        self.event_rx.try_iter().collect()
    }
}

struct Queued {
    priority: Priority,
    generation: u64,
    seq: u64,
}

#[derive(Default)]
struct Queue {
    items: HashMap<Scope, Queued>,
    seq: u64,
}

impl Queue {
    fn push(&mut self, req: FetchRequest) {
        self.seq += 1;
        let seq = self.seq;
        self.items
            .entry(req.scope)
            .and_modify(|q| {
                q.priority = q.priority.min(req.priority);
                q.generation = q.generation.max(req.generation);
            })
            .or_insert(Queued {
                priority: req.priority,
                generation: req.generation,
                seq,
            });
    }

    fn pop(&mut self) -> Option<FetchRequest> {
        let scope = self
            .items
            .iter()
            .min_by_key(|(_, q)| (q.priority, q.seq))
            .map(|(s, _)| s.clone())?;
        let q = self.items.remove(&scope)?;
        Some(FetchRequest {
            scope,
            generation: q.generation,
            priority: q.priority,
        })
    }
}

fn run(
    mut source: Box<dyn MetadataSource + Send>,
    req_rx: Receiver<FetchRequest>,
    event_tx: Sender<WorkerEvent>,
) {
    let connected = match source.identify() {
        Ok((key, current)) => WorkerEvent::Connected { key, current },
        Err(e) => WorkerEvent::ConnectFailed(format!("{e:#}")),
    };
    let failed = matches!(connected, WorkerEvent::ConnectFailed(_));
    if event_tx.send(connected).is_err() || failed {
        return;
    }

    let mut queue = Queue::default();
    loop {
        // Drain everything that arrived during the last fetch so a Demand can
        // overtake queued Crawl work.
        if !drain(&req_rx, &mut queue) {
            return;
        }
        let Some(mut req) = queue.pop() else {
            match req_rx.recv() {
                Ok(req) => queue.push(req),
                Err(_) => return,
            }
            continue;
        };
        if req.priority == Priority::Crawl {
            queue.push(req);
            thread::sleep(CRAWL_INTERVAL);
            if !drain(&req_rx, &mut queue) {
                return;
            }
            req = queue.pop().expect("the crawl request was just queued");
        }
        let result = source.fetch(&req.scope).map_err(|e| format!("{e:#}"));
        let patch = CatalogPatch {
            scope: req.scope,
            generation: req.generation,
            fetched_at: SystemTime::now(),
            result,
        };
        if event_tx.send(WorkerEvent::Patch(patch)).is_err() {
            return;
        }
    }
}

/// Returns false once the UI side has gone away.
fn drain(req_rx: &Receiver<FetchRequest>, queue: &mut Queue) -> bool {
    loop {
        match req_rx.try_recv() {
            Ok(req) => queue.push(req),
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Children, Name};
    use color_eyre::Result;
    use std::sync::{Arc, Mutex};

    struct Fake {
        gate: Arc<Mutex<()>>,
        entered: Sender<()>,
        log: Arc<Mutex<Vec<Scope>>>,
    }

    impl MetadataSource for Fake {
        fn identify(&mut self) -> Result<(CatalogKey, CurrentScope)> {
            Ok((
                CatalogKey {
                    account: "A".into(),
                    role: Name::new("R"),
                },
                CurrentScope::default(),
            ))
        }

        fn fetch(&mut self, scope: &Scope) -> Result<Children> {
            let _ = self.entered.send(());
            let _held = self.gate.lock().unwrap();
            self.log.lock().unwrap().push(scope.clone());
            Ok(Children::Databases(vec![]))
        }
    }

    fn db(n: &str) -> Scope {
        Scope::Database(Name::new(n))
    }

    fn wait_patches(w: &MetaWorker, n: usize) -> Vec<CatalogPatch> {
        let mut out = Vec::new();
        for _ in 0..500 {
            for e in w.poll() {
                if let WorkerEvent::Patch(p) = e {
                    out.push(p);
                }
            }
            if out.len() >= n {
                return out;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {n} patches, got {}", out.len());
    }

    #[test]
    fn serves_higher_priority_first_and_merges_duplicates() {
        let gate = Arc::new(Mutex::new(()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (entered, fetch_started) = mpsc::channel();
        let w = MetaWorker::spawn_with(Box::new(Fake {
            gate: gate.clone(),
            entered,
            log: log.clone(),
        }));

        let hold = gate.lock().unwrap();
        w.request(FetchRequest {
            scope: db("FIRST"),
            generation: 0,
            priority: Priority::Crawl,
        });
        fetch_started.recv().unwrap();
        w.request(FetchRequest {
            scope: db("CRAWL"),
            generation: 0,
            priority: Priority::Crawl,
        });
        w.request(FetchRequest {
            scope: db("DEMAND"),
            generation: 0,
            priority: Priority::Context,
        });
        w.request(FetchRequest {
            scope: db("DEMAND"),
            generation: 2,
            priority: Priority::Demand,
        });
        drop(hold);

        let patches = wait_patches(&w, 3);
        assert_eq!(
            *log.lock().unwrap(),
            vec![db("FIRST"), db("DEMAND"), db("CRAWL")]
        );
        let demand = patches.iter().find(|p| p.scope == db("DEMAND")).unwrap();
        assert_eq!(demand.generation, 2);
    }
}
