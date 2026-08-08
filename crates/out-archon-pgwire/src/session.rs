//! Per-connection session state: transaction status, parameter tracking, and the
//! advisory `pid`/`secret` used in PostgreSQL's `BackendKeyData`.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use std::sync::{Arc, Mutex};

use tpt_archon_relational::database::{Database, SessionId};

use crate::metrics::{Metrics, SharedMetrics};
use crate::startup::scram::{AuthStore, ScramExchange};

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
    /// Stable per-connection identity used to scope transactions in the
    /// relational `Database` (per-session MVCC isolation).
    pub id: SessionId,
    /// Shared, lock-free server metrics, incremented as statements are executed
    /// and errors returned on this connection.
    pub metrics: SharedMetrics,
    /// Credential store used to offer SCRAM-SHA-256 during startup. `None`
    /// means trust auth (no password challenge).
    pub auth_store: Option<Arc<AuthStore>>,
    /// In-progress SCRAM exchange state across the SASL messages.
    pub scram_state: Option<ScramExchange>,
    /// Per-statement timeout in milliseconds (`None` = unlimited). Set via
    /// `SET statement_timeout`. Enforcement is best-effort in the synchronous
    /// path (see `simple_query`); full preemption needs the async scheduler.
    pub statement_timeout_ms: Option<u64>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            txn_status: SessionTxnStatus::Idle,
            pid: 0,
            secret: 0,
            params: Vec::new(),
            database: Arc::new(Mutex::new(Database::empty())),
            auth_store: None,
            scram_state: None,
            statement_timeout_ms: None,
            id: SessionId::new(),
            metrics: Arc::new(Metrics::new()),
        }
    }

    pub fn with_database(database: Arc<Mutex<Database>>) -> Self {
        Self {
            txn_status: SessionTxnStatus::Idle,
            pid: 0,
            secret: 0,
            params: Vec::new(),
            database,
            auth_store: None,
            scram_state: None,
            statement_timeout_ms: None,
            id: SessionId::new(),
            metrics: Arc::new(Metrics::new()),
        }
    }

    /// Creates a session bound to a credential store (enables SCRAM-SHA-256).
    pub fn with_auth_store(database: Arc<Mutex<Database>>, auth_store: Arc<AuthStore>) -> Self {
        let mut s = Self::with_database(database);
        s.auth_store = Some(auth_store);
        s
    }

    /// Parses a PostgreSQL `statement_timeout` value into milliseconds.
    ///
    /// Accepts plain milliseconds (`"5000"`), a unit suffix (`"5s"`, `"100ms"`,
    /// `"2min"`), or `"0"`/`"off"` (disable). Returns `None` on an unparseable
    /// value.
    pub fn parse_statement_timeout(value: &str) -> Option<u64> {
        let v = value.trim().to_ascii_lowercase();
        if v.is_empty() || v == "off" || v == "0" || v == "0ms" {
            return Some(0);
        }
        let (num, unit) = match v.find(|c: char| !c.is_ascii_digit()) {
            Some(idx) => (v[..idx].trim(), v[idx..].trim()),
            None => (v.as_str(), ""),
        };
        let n: u64 = num.parse().ok()?;
        let ms = match unit {
            "" | "ms" => n,
            "s" => n.saturating_mul(1000),
            "min" | "m" => n.saturating_mul(60_000),
            _ => return None,
        };
        Some(ms)
    }

    /// Sets the per-statement timeout (milliseconds). `0` disables it.
    pub fn set_statement_timeout(&mut self, ms: u64) {
        self.statement_timeout_ms = if ms == 0 { None } else { Some(ms) };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_statement_timeout_variants() {
        assert_eq!(Session::parse_statement_timeout("0"), Some(0));
        assert_eq!(Session::parse_statement_timeout("off"), Some(0));
        assert_eq!(Session::parse_statement_timeout("5000"), Some(5000));
        assert_eq!(Session::parse_statement_timeout("5s"), Some(5000));
        assert_eq!(Session::parse_statement_timeout("100ms"), Some(100));
        assert_eq!(Session::parse_statement_timeout("2min"), Some(120_000));
        assert_eq!(Session::parse_statement_timeout(" 3 s "), Some(3000));
        assert_eq!(Session::parse_statement_timeout("bogus"), None);
    }

    #[test]
    fn set_statement_timeout_toggles_none() {
        let mut s = Session::new();
        s.set_statement_timeout(1000);
        assert_eq!(s.statement_timeout_ms, Some(1000));
        s.set_statement_timeout(0);
        assert_eq!(s.statement_timeout_ms, None);
    }
}
