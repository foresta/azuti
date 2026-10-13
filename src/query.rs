use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use arrow::array::RecordBatch;

use crate::snowflake::{self, SnowflakeParams};

/// The result of a submitted query: Arrow batches, or an error message.
pub type QueryOutcome = Result<Vec<RecordBatch>, String>;

/// A background worker that runs Snowflake queries off the UI thread, so the TUI
/// stays responsive while a query is in flight.
#[derive(Debug)]
pub struct QueryEngine {
    sql_tx: Sender<String>,
    result_rx: Receiver<QueryOutcome>,
    _handle: JoinHandle<()>,
}

impl QueryEngine {
    /// Spawn the worker. Each submitted query connects, runs, and returns Arrow
    /// batches (connection reuse is a later optimization).
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

    /// Queue a query to run. The result arrives later via [`QueryEngine::poll`].
    pub fn submit(&self, sql: String) {
        let _ = self.sql_tx.send(sql);
    }

    /// Non-blocking check for a finished query.
    pub fn poll(&self) -> Option<QueryOutcome> {
        self.result_rx.try_recv().ok()
    }
}

fn worker(params: SnowflakeParams, sql_rx: Receiver<String>, result_tx: Sender<QueryOutcome>) {
    while let Ok(sql) = sql_rx.recv() {
        let outcome = snowflake::run_query(&params, &sql).map_err(|e| format!("{e:#}"));
        if result_tx.send(outcome).is_err() {
            break; // the UI is gone
        }
    }
}
