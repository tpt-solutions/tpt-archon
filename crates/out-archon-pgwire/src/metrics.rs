//! In-process metrics for the Archon PostgreSQL-wire server.
//!
//! A lightweight, allocation-free counter surface suitable for exposing
//! connection/query/error throughput to an operator (read
//! [`Metrics::snapshot`] from the running [`Server`](crate::server::Server), or
//! wire it into a `/metrics` endpoint in a hosting process). Counters are
//! `AtomicU64` so they can be incremented from per-connection threads without a
//! mutex; the snapshot is taken on demand.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cumulative, lock-free server metrics.
#[derive(Debug)]
pub struct Metrics {
    connections_accepted: AtomicU64,
    connections_closed: AtomicU64,
    statements_executed: AtomicU64,
    errors: AtomicU64,
    bytes_sent: AtomicU64,
    started_at: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            connections_accepted: AtomicU64::new(0),
            connections_closed: AtomicU64::new(0),
            statements_executed: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            started_at: Instant::now(),
        }
    }

    pub fn record_connection_accepted(&self) {
        self.connections_accepted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_connection_closed(&self) {
        self.connections_closed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_statement(&self) {
        self.statements_executed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bytes_sent(&self, n: u64) {
        self.bytes_sent.fetch_add(n, Ordering::Relaxed);
    }

    /// Returns a point-in-time copy of the counters plus uptime.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            connections_accepted: self.connections_accepted.load(Ordering::Relaxed),
            connections_closed: self.connections_closed.load(Ordering::Relaxed),
            statements_executed: self.statements_executed.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            uptime: self.started_at.elapsed(),
        }
    }
}

/// A point-in-time, owned copy of [`Metrics`] safe to serialize / log.
#[derive(Debug, Clone, Copy)]
pub struct MetricsSnapshot {
    pub connections_accepted: u64,
    pub connections_closed: u64,
    pub statements_executed: u64,
    pub errors: u64,
    pub bytes_sent: u64,
    pub uptime: Duration,
}

impl MetricsSnapshot {
    /// Renders the snapshot as a Prometheus-style text exposition block.
    pub fn as_prometheus(&self) -> String {
        format!(
            "# HELP archon_connections_accepted Total accepted connections\n\
             # TYPE archon_connections_accepted counter\n\
             archon_connections_accepted {}\n\
             # HELP archon_connections_closed Total closed connections\n\
             # TYPE archon_connections_closed counter\n\
             archon_connections_closed {}\n\
             # HELP archon_statements_executed Total statements executed\n\
             # TYPE archon_statements_executed counter\n\
             archon_statements_executed {}\n\
             # HELP archon_errors Total errors returned\n\
             # TYPE archon_errors counter\n\
             archon_errors {}\n\
             # HELP archon_bytes_sent Total bytes sent to clients\n\
             # TYPE archon_bytes_sent counter\n\
             archon_bytes_sent {}\n",
            self.connections_accepted,
            self.connections_closed,
            self.statements_executed,
            self.errors,
            self.bytes_sent,
        )
    }
}

/// Convenience: an `Arc<Metrics>` shared across connection threads.
pub type SharedMetrics = Arc<Metrics>;
