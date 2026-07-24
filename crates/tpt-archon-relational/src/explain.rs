//! `EXPLAIN` support: render a query's plan and (with the `gpu` feature) the
//! TPTIR the GPU path would emit.
//!
//! `explain_plan` is always available and renders the physical plan the
//! planner produced, including its dispatch decision. `explain_gpu` is gated
//! behind the `gpu` feature: when a plan dispatches to [`Dispatch::Gpu`] it
//! shows the stable TPTIR text that an external GPU backend would consume.
//! There is no GPU execution here — only emission.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::parser::{parse_select, ParseError};
use crate::planner::{plan_select, Dispatch, PlanNode, TableStats};

/// Renders a `SELECT` statement's physical plan as a readable multi-line string,
/// including the estimated row count and the chosen dispatch target.
pub fn explain_plan(sql: &str, stats: TableStats) -> Result<String, ParseError> {
    let stmt = parse_select(sql)?;
    let plan = plan_select(&stmt, stats);
    let mut out = String::new();
    out.push_str("Plan:\n");
    render_node(&plan.root, 1, &mut out);
    out.push_str(&format!("estimated_rows: {}\n", plan.estimated_rows));
    out.push_str(&format!(
        "dispatch: {}\n",
        match plan.dispatch {
            Dispatch::Cpu => "CPU",
            Dispatch::Gpu => "GPU (TPTIR emission)",
        }
    ));
    Ok(out)
}

fn render_node(node: &PlanNode, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match node {
        PlanNode::Scan { table, vectorized } => {
            out.push_str(&format!(
                "{indent}Scan {{ table: {table}, vectorized: {vectorized} }}\n"
            ));
        }
        PlanNode::Filter { expr, input } => {
            out.push_str(&format!("{indent}Filter {{ {expr:?} }}\n"));
            render_node(input, depth + 1, out);
        }
        PlanNode::Project {
            columns,
            star,
            input,
        } => {
            let cols = if *star {
                "*".to_string()
            } else {
                columns.join(", ")
            };
            out.push_str(&format!("{indent}Project {{ {cols} }}\n"));
            render_node(input, depth + 1, out);
        }
        PlanNode::Limit { n, input } => {
            out.push_str(&format!("{indent}Limit {{ n: {n} }}\n"));
            render_node(input, depth + 1, out);
        }
        PlanNode::Sort { columns, input } => {
            let cols: Vec<String> = columns
                .iter()
                .map(|ob| {
                    if ob.descending {
                        alloc::format!("{} DESC", ob.column)
                    } else {
                        alloc::format!("{} ASC", ob.column)
                    }
                })
                .collect();
            out.push_str(&format!("{indent}Sort {{ {} }}\n", cols.join(", ")));
            render_node(input, depth + 1, out);
        }
        PlanNode::Aggregate {
            group_by,
            aggregates,
            having,
            input,
        } => {
            let gb = if group_by.is_empty() {
                "none".to_string()
            } else {
                group_by.join(", ")
            };
            let aggs: Vec<String> = aggregates
                .iter()
                .map(|(alias, func, col)| alloc::format!("{func:?}({col}) AS {alias}"))
                .collect();
            let hv = having
                .as_ref()
                .map(|e| alloc::format!(", having: {e:?}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "{indent}Aggregate {{ group_by: [{gb}], aggs: [{}]{hv} }}\n",
                aggs.join(", ")
            ));
            render_node(input, depth + 1, out);
        }
        PlanNode::SubqueryScan { plan, alias } => {
            out.push_str(&format!("{indent}SubqueryScan {{ alias: {alias} }}\n"));
            render_node(&plan.root, depth + 1, out);
        }
    }
}

/// The number of embeddings a GPU top-k scan would operate on for `plan`.
///
/// Used to size the emitted TPTIR region. We use the estimated row count as the
/// table length (the scan touches that many embedding rows).
#[cfg(feature = "gpu")]
use crate::planner::Plan;

#[cfg(feature = "gpu")]
fn scan_rows(plan: &Plan) -> u64 {
    plan.estimated_rows.max(1)
}

/// Renders a `SELECT` statement's plan together with the TPTIR the GPU path
/// would emit for its scan. Returns an error if the statement fails to parse.
///
/// This is the visible, demo-able face of the GPU integration: it shows the
/// *emitted* IR, not any device execution. Only built with the `gpu` feature.
#[cfg(feature = "gpu")]
pub fn explain_gpu(sql: &str, stats: TableStats) -> Result<String, ParseError> {
    let stmt = parse_select(sql)?;
    let plan = plan_select(&stmt, stats);
    let mut out = explain_plan(sql, stats)?;
    out.push('\n');
    if plan.dispatch == Dispatch::Gpu {
        out.push_str("GPU IR (TPTIR, emitted — not executed):\n");
        out.push_str(&crate::gpu::emit_topk(scan_rows(&plan)));
    } else {
        out.push_str(
            "GPU IR: not emitted (dispatch is CPU; build with `gpu` feature and a \
             large enough scan to enable GPU dispatch).\n",
        );
    }
    Ok(out)
}

/// A convenience used by the `gpu` example/test path when no SQL is handy: emit
/// TPTIR for a top-k scan over `n` embeddings directly.
#[cfg(feature = "gpu")]
pub fn explain_gpu_scan(n: u64) -> String {
    crate::gpu::emit_topk(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_plan_renders_tree_and_cpu_dispatch() {
        let s = explain_plan(
            "SELECT id FROM users WHERE age >= 25 LIMIT 3",
            TableStats { row_count: 100 },
        )
        .unwrap();
        assert!(s.contains("Project { id }"));
        assert!(s.contains("Scan { table: users"));
        assert!(s.contains("Limit { n: 3 }"));
        assert!(s.contains("dispatch: CPU"));
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn explain_gpu_emits_tptir_for_large_scan() {
        let s = explain_gpu(
            "SELECT * FROM huge",
            TableStats {
                row_count: 2_000_000,
            },
        )
        .unwrap();
        assert!(s.contains("func @vector_topk"));
    }
}
