//! A vectorized execution engine.
//!
//! Executes a [`Plan`](crate::planner::Plan) against an in-memory [`Table`],
//! processing rows in batches ([`BATCH_SIZE`]) rather than one at a time.
//! This is the CPU-only path; GPU offload (via `tpt-gpu-*`, behind the `gpu`
//! feature) plugs into the same dispatch decision the planner already makes
//! but is not required — every query has a working CPU fallback.
//!
//! Submodules mirror the file's original sections: [`value`] (the row/table
//! data model and literal conversion), [`expr`] (`WHERE`/`HAVING` expression
//! evaluation), [`aggregate`] (`GROUP BY` + aggregate functions), [`exec`]
//! (walking a [`PlanNode`](crate::planner::PlanNode) tree), and [`vector`]
//! (embedding similarity search). Only the items re-exported below are part
//! of the crate's public surface; everything else is private to `executor`
//! (or `pub(crate)` where [`database`](crate::database) or
//! [`parser`](crate::parser) need it directly).

mod aggregate;
mod exec;
mod expr;
#[cfg(test)]
mod tests;
mod value;
mod vector;

/// Rows processed per vectorized batch.
pub const BATCH_SIZE: usize = 1024;

pub(crate) use aggregate::agg_default_alias;
pub use aggregate::aggregate_table;
pub use exec::execute;
pub(crate) use expr::find_value;
pub use expr::{eval_expr, eval_expr_scoped, eval_scalar};
pub use value::*;
pub use vector::vector_topk;
