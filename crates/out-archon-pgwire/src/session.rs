//! Per-connection session state: transaction status, parameter tracking, and the
//! advisory `pid`/`secret` used in PostgreSQL's `BackendKeyData`.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use std::sync::{Arc, Mutex};

use tpt_archon_relational::database::Database;

/// Transaction status for the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionTxnStatus {
    #[default]
    Idle,
    InTransaction,
    Failed,
}

impl SessionTxnStatus {
    pub fn as_byte(&self) -> u8 {
        match self {
            SessionTxnStatus::Idle => b'I',
            SessionTxnStatus::InTransaction => b'T',
            SessionTxnStatus::Failed => b'E',
        }
    }

    pub fn to_transaction_status(&self) -> crate::codec::message::TxnStatus {
        match self {
            SessionTxnStatus::Idle => crate::codec::message::TxnStatus::Idle,
            SessionTxnStatus::InTransaction => crate::codec::message::TxnStatus::InTransaction,
            SessionTxnStatus::Failed => crate::codec::message::TxnStatus::Failed,
        }
    }
}

/// Per-connection state tracked by the PostgreSQL wire handler.
#[derive(Debug, Clone)]
pub struct Session {
    pub txn_status: SessionTxnStatus,
    pub pid: i32,
    pub secret: i32,
    pub params: Vec<(String, String)>,
    pub database: Arc<Mutex<Database>>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            txn_status: SessionTxnStatus::Idle,
            pid: 0,
            secret: 0,
            params: Vec::new(),
            database: Arc::new(Mutex::new(Database::empty())),
        }
    }

    pub fn with_database(database: Arc<Mutex<Database>>) -> Self {
        Self {
            txn_status: SessionTxnStatus::Idle,
            pid: 0,
            secret: 0,
            params: Vec::new(),
            database,
        }
    }

    pub fn begin(&mut self) {
        self.txn_status = SessionTxnStatus::InTransaction;
    }

    pub fn commit(&mut self) {
        self.txn_status = SessionTxnStatus::Idle;
    }

    pub fn rollback(&mut self) {
        self.txn_status = SessionTxnStatus::Failed;
    }

    pub fn set_failed(&mut self) {
        self.txn_status = SessionTxnStatus::Failed;
    }

    pub fn mark_transaction_failed(&mut self) {
        self.txn_status = SessionTxnStatus::Failed;
    }

    pub fn set_parameter(&mut self, key: String, value: String) {
        self.params.retain(|(k, _)| k != &key);
        self.params.push((key, value));
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
