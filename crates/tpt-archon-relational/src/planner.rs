//! A small cost-based query planner.
//!
//! Turns a parsed [`SelectStatement`] into a [`PlanNode`] tree the executor can
//! run. The planner applies one real optimization decision — whether to push
//! the filter below the projection and whether the workload looks analytical
//! enough to prefer a vectorized (batched) scan — and records an estimated cost
//! so a cost model can later choose CPU vs GPU dispatch.
//!
//! Statistics are supplied via [`TableStats`]; the cost estimate is
//! `rows_scanned` adjusted by selectivity, which is enough to drive the
//! vectorization and (future) GPU-offload decision.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::parser::{Predicate, SelectStatement};

/// Coarse statistics about a table, used for cost estimation.
#[derive(Debug, Clone, Copy)]
pub struct TableStats {
    /// Estimated number of rows in the table.
    pub row_count: u64,
}

/// Where a node prefers to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Row/batch execution on the CPU.
    Cpu,
    /// Offload to GPU (only chosen when the `gpu` feature is enabled and the
    /// estimated batch is large enough to amortize transfer).
    Gpu,
}

/// A physical plan node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanNode {
    /// Full scan of a table.
    Scan {
        /// Table name.
        table: String,
        /// Whether the scan runs in vectorized (batched) mode.
        vectorized: bool,
    },
    /// Filter rows by a predicate.
    Filter {
        /// The predicate to apply.
        predicate: Predicate,
        /// Input node.
        input: Box<PlanNode>,
    },
    /// Project a subset of columns (empty + star = all columns).
    Project {
        /// Columns to keep.
        columns: Vec<String>,
        /// Whether to keep all columns.
        star: bool,
        /// Input node.
        input: Box<PlanNode>,
    },
    /// Limit the number of output rows.
    Limit {
        /// Maximum rows.
        n: u64,
        /// Input node.
        input: Box<PlanNode>,
    },
}

/// A plan plus its estimated cost and dispatch decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The root plan node.
    pub root: PlanNode,
    /// Estimated rows processed (cost proxy).
    pub estimated_rows: u64,
    /// Chosen execution target.
    pub dispatch: Dispatch,
}

/// Threshold above which an analytical scan is worth vectorizing.
const VECTORIZE_ROW_THRESHOLD: u64 = 1024;
/// Threshold above which GPU offload is considered (needs `gpu` feature too).
const GPU_ROW_THRESHOLD: u64 = 1_000_000;

/// Estimates predicate selectivity as a `(numerator, denominator)` fraction of
/// rows kept. Integer math keeps this `no_std`-clean (no `f64::ceil`).
fn selectivity(pred: &Predicate) -> (u64, u64) {
    use crate::parser::CmpOp::*;
    match pred.op {
        Eq => (1, 10),
        Ne => (9, 10),
        Lt | Le | Gt | Ge => (1, 3),
    }
}

/// Plans a `SELECT` against the given table statistics.
pub fn plan_select(stmt: &SelectStatement, stats: TableStats) -> Plan {
    let vectorized = stats.row_count >= VECTORIZE_ROW_THRESHOLD;
    let mut node = PlanNode::Scan {
        table: stmt.table.clone(),
        vectorized,
    };

    let mut estimated = stats.row_count;

    // Push filter directly above the scan (before projection).
    if let Some(pred) = &stmt.filter {
        let (num, den) = selectivity(pred);
        // Ceiling division to keep at least one estimated row.
        estimated = (estimated * num).div_ceil(den);
        node = PlanNode::Filter {
            predicate: pred.clone(),
            input: Box::new(node),
        };
    }

    node = PlanNode::Project {
        columns: stmt.columns.clone(),
        star: stmt.star,
        input: Box::new(node),
    };

    if let Some(n) = stmt.limit {
        estimated = estimated.min(n);
        node = PlanNode::Limit {
            n,
            input: Box::new(node),
        };
    }

    // Cost model: choose GPU only if built with the feature and the scan is
    // large enough to amortize offload; otherwise CPU.
    let dispatch = if cfg!(feature = "gpu") && stats.row_count >= GPU_ROW_THRESHOLD {
        Dispatch::Gpu
    } else {
        Dispatch::Cpu
    };

    Plan {
        root: node,
        estimated_rows: estimated,
        dispatch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_select;

    #[test]
    fn plans_scan_project_for_simple_select() {
        let stmt = parse_select("SELECT id FROM t").unwrap();
        let plan = plan_select(&stmt, TableStats { row_count: 10 });
        assert!(matches!(plan.root, PlanNode::Project { .. }));
        assert_eq!(plan.dispatch, Dispatch::Cpu);
        assert_eq!(plan.estimated_rows, 10);
    }

    #[test]
    fn filter_reduces_estimated_rows() {
        let stmt = parse_select("SELECT * FROM t WHERE x = 5").unwrap();
        let plan = plan_select(&stmt, TableStats { row_count: 1000 });
        assert!(plan.estimated_rows < 1000);
    }

    #[test]
    fn large_tables_are_vectorized() {
        let stmt = parse_select("SELECT * FROM big").unwrap();
        let plan = plan_select(&stmt, TableStats { row_count: 10_000 });
        // The innermost scan should be vectorized.
        if let PlanNode::Project { input, .. } = &plan.root {
            assert!(matches!(
                **input,
                PlanNode::Scan {
                    vectorized: true,
                    ..
                }
            ));
        } else {
            panic!("expected project at root");
        }
    }

    #[test]
    fn limit_caps_estimate() {
        let stmt = parse_select("SELECT * FROM t LIMIT 3").unwrap();
        let plan = plan_select(&stmt, TableStats { row_count: 1000 });
        assert_eq!(plan.estimated_rows, 3);
    }

    #[test]
    fn cpu_dispatch_without_gpu_feature() {
        let stmt = parse_select("SELECT * FROM huge").unwrap();
        let plan = plan_select(
            &stmt,
            TableStats {
                row_count: 5_000_000,
            },
        );
        // Without the gpu feature, always CPU.
        if !cfg!(feature = "gpu") {
            assert_eq!(plan.dispatch, Dispatch::Cpu);
        }
    }
}
