use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use arrow::array::RecordBatch;

use crate::connection::{Engine, QueryOutcome};
use crate::snowflake::{Session, SnowflakeParams};

/// A background worker that runs Snowflake queries off the UI thread, so the TUI
/// stays responsive while a query is in flight.
#[derive(Debug)]
pub struct SnowflakeEngine {
    sql_tx: Sender<String>,
    result_rx: Receiver<QueryOutcome>,
    _handle: JoinHandle<()>,
}

impl SnowflakeEngine {
    /// Spawn the worker. It keeps one session for its lifetime and reconnects on
    /// the next query if building the session failed.
    pub fn spawn(params: SnowflakeParams) -> Self {
        let (sql_tx, sql_rx) = mpsc::channel::<String>();
        let (result_tx, result_rx) = mpsc::channel::<QueryOutcome>();
        let handle = thread::spawn(move || worker(params, sql_rx, result_tx));
        Self {
            sql_tx,
            result_rx,
            _handle: handle,
        }
    }
}

impl Engine for SnowflakeEngine {
    fn submit(&self, sql: String) {
        let _ = self.sql_tx.send(sql);
    }

    fn poll(&self) -> Option<QueryOutcome> {
        self.result_rx.try_recv().ok()
    }
}

fn worker(params: SnowflakeParams, sql_rx: Receiver<String>, result_tx: Sender<QueryOutcome>) {
    let mut session: Option<Session> = None;
    while let Ok(sql) = sql_rx.recv() {
        let outcome = run(&params, &mut session, &sql).map_err(|e| format!("{e:#}"));
        if result_tx.send(outcome).is_err() {
            break; // the UI is gone
        }
    }
}

fn run(
    params: &SnowflakeParams,
    session: &mut Option<Session>,
    sql: &str,
) -> color_eyre::Result<Vec<RecordBatch>> {
    let session = match session {
        Some(s) => s,
        None => session.insert(Session::connect(params)?),
    };
    session.exec(sql)?.into_batches()
}
