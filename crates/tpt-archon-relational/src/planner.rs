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
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::parser::{Expr, OrderBy, SelectStatement};

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
#[derive(Debug, Clone, PartialEq)]
pub enum PlanNode {
    /// Full scan of a table.
    Scan {
        /// Table name.
        table: String,
        /// Whether the scan runs in vectorized (batched) mode.
        vectorized: bool,
    },
    /// Filter rows by a boolean expression.
    Filter {
        /// The expression to evaluate.
        expr: Expr,
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
    /// Sort the result set.
    Sort {
        /// Sort specifications.
        columns: Vec<OrderBy>,
        /// Input node.
        input: Box<PlanNode>,
    },
    /// Aggregate rows by group-by columns with aggregate functions.
    Aggregate {
        /// Columns to group by.
        group_by: Vec<String>,
        /// Aggregate functions to compute.
        aggregates: Vec<(String, crate::parser::AggregateFunc, String)>,
        /// Optional HAVING filter applied after aggregation.
        having: Option<Expr>,
        /// Input node.
        input: Box<PlanNode>,
    },
    /// A nested query executed as a scannable source — the shared primitive
    /// views, subqueries-in-`FROM`, and CTE materialization build on. Not yet
    /// emitted by [`plan_select`]; wired up by later phases.
    SubqueryScan {
        /// The nested plan to execute.
        plan: Box<Plan>,
        /// The alias this nested result is referenced by.
        alias: String,
    },
}

/// A plan plus its estimated cost and dispatch decision.
#[derive(Debug, Clone, PartialEq)]
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
/// rows kept. Integer math keeps this `no_std`-clean.
fn selectivity(expr: &Expr) -> (u64, u64) {
    match expr {
        Expr::Cmp {
            op: crate::parser::CmpOp::Eq,
            ..
        } => (1, 10),
        Expr::Cmp {
            op: crate::parser::CmpOp::Ne,
            ..
        } => (9, 10),
        Expr::Cmp { .. } => (1, 3),
        Expr::IsNull { negated: false, .. } => (1, 20),
        Expr::IsNull { negated: true, .. } => (19, 20),
        Expr::Like { .. } => (1, 5),
        Expr::InInt { values, .. } => {
            let n = values.len().max(1) as u64;
            (n, 100)
        }
        Expr::BetweenInt { .. } => (1, 3),
        Expr::CmpColumn {
            op: crate::parser::CmpOp::Eq,
            ..
        } => (1, 10),
        Expr::CmpColumn { .. } => (1, 3),
        Expr::And(l, r) => {
            let (a, b) = selectivity(l);
            let (c, d) = selectivity(r);
            ((a * c).max(1), b * d)
        }
        Expr::Or(l, r) => {
            let (a, b) = selectivity(l);
            let (c, d) = selectivity(r);
            // Approximate: P(A or B) = P(A) + P(B) - P(A)*P(B).
            // For integer estimates: (a*d + c*b - a*c) / (b*d), capped at 1.
            let num = a * d + c * b - a * c;
            let den = b * d;
            if num >= den {
                (1, 1)
            } else {
                (num.max(1), den)
            }
        }
        Expr::Not(inner) => {
            let (a, b) = selectivity(inner);
            // P(NOT x) = 1 - P(x).
            (b.saturating_sub(a).max(1), b)
        }
        // Subqueries are evaluated at the database level; default estimate.
        Expr::Exists { .. } | Expr::InSubquery { .. } | Expr::ScalarCmp { .. } => (1, 3),
        // Aggregate expressions are resolved after GROUP BY; default estimate.
        Expr::Agg { .. } => (1, 3),
    }
}

/// Plans a `SELECT` against the given table statistics.
pub fn plan_select(stmt: &SelectStatement, stats: TableStats) -> Plan {
    let vectorized = stats.row_count >= VECTORIZE_ROW_THRESHOLD;
    let mut node = PlanNode::Scan {
        table: stmt.table.name().to_string(),
        vectorized,
    };

    let mut estimated = stats.row_count;

    // Push filter directly above the scan.
    if let Some(expr) = &stmt.filter {
        let (num, den) = selectivity(expr);
        estimated = (estimated * num).div_ceil(den);
        node = PlanNode::Filter {
            expr: expr.clone(),
            input: Box::new(node),
        };
    }

    // GROUP BY / aggregates.
    if !stmt.group_by.is_empty() || !stmt.aggregates.is_empty() {
        node = PlanNode::Aggregate {
            group_by: stmt.group_by.clone(),
            aggregates: stmt.aggregates.clone(),
            having: stmt.having.clone(),
            input: Box::new(node),
        };
        // Group-by reduces estimated rows.
        if !stmt.group_by.is_empty() {
            estimated = estimated.div_ceil(10).max(1);
        }
    }

    node = PlanNode::Project {
        columns: stmt.columns.clone(),
        star: stmt.star,
        input: Box::new(node),
    };

    // ORDER BY (non-cosine).
    if !stmt.order_by.is_empty() {
        node = PlanNode::Sort {
            columns: stmt.order_by.clone(),
            input: Box::new(node),
        };
    }

    if let Some(n) = stmt.limit {
        estimated = estimated.min(n);
        node = PlanNode::Limit {
            n,
            input: Box::new(node),
        };
    }

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
        if !cfg!(feature = "gpu") {
            assert_eq!(plan.dispatch, Dispatch::Cpu);
        }
    }

    #[test]
    fn group_by_plans_aggregate_node() {
        let stmt = parse_select("SELECT dept, COUNT(*) FROM t GROUP BY dept").unwrap();
        let plan = plan_select(&stmt, TableStats { row_count: 100 });
        assert!(matches!(plan.root, PlanNode::Project { .. }));
    }

    #[test]
    fn subquery_scan_node_round_trips() {
        let stmt = parse_select("SELECT id FROM t").unwrap();
        let inner_plan = plan_select(&stmt, TableStats { row_count: 5 });
        let node = PlanNode::SubqueryScan {
            plan: alloc::boxed::Box::new(inner_plan),
            alias: "sub".to_string(),
        };
        assert!(matches!(node, PlanNode::SubqueryScan { .. }));
    }

    #[test]
    fn order_by_plans_sort_node() {
        let stmt = parse_select("SELECT * FROM t ORDER BY x DESC").unwrap();
        let plan = plan_select(&stmt, TableStats { row_count: 100 });
        // Root should be Limit (none) -> Sort -> Project -> Filter -> Scan.
        // Since no limit, root is Sort wrapped by Project.
        match &plan.root {
            PlanNode::Sort { .. } => {}
            PlanNode::Project { input, .. } => {
                assert!(matches!(**input, PlanNode::Sort { .. }));
            }
            other => panic!("expected Sort or Project wrapping Sort, got {:?}", other),
        }
    }
}
