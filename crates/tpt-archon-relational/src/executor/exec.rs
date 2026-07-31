//! Walking a [`PlanNode`] tree to produce a [`ResultSet`].

use alloc::vec::Vec;

use crate::planner::{Plan, PlanNode};

use super::aggregate::aggregate_table;
use super::expr::eval_expr;
use super::value::{ExecError, ResultSet, Table};
use super::BATCH_SIZE;

/// Executes `plan` against `table`.
pub fn execute(plan: &Plan, table: &Table) -> Result<ResultSet, ExecError> {
    execute_node(&plan.root, table)
}

fn execute_node(node: &PlanNode, table: &Table) -> Result<ResultSet, ExecError> {
    match node {
        PlanNode::Scan { .. } => {
            let mut rows = Vec::with_capacity(table.rows.len());
            for chunk in table.rows.chunks(BATCH_SIZE) {
                rows.extend_from_slice(chunk);
            }
            Ok(ResultSet {
                columns: table.columns.clone(),
                rows: table.rows.clone(),
                ..Default::default()
            })
        }
        PlanNode::Filter { expr, input } => {
            let inner = execute_node(input, table)?;
            let mut rows = Vec::new();
            for chunk in inner.rows.chunks(BATCH_SIZE) {
                for row in chunk {
                    if eval_expr(expr, &inner.columns, row)? {
                        rows.push(row.clone());
                    }
                }
            }
            Ok(ResultSet {
                columns: inner.columns,
                rows,
                ..Default::default()
            })
        }
        PlanNode::Project {
            columns,
            star,
            input,
        } => {
            let inner = execute_node(input, table)?;
            if *star || columns.is_empty() {
                return Ok(inner);
            }
            let indices: Vec<usize> = columns
                .iter()
                .map(|c| {
                    inner
                        .columns
                        .iter()
                        .position(|ic| ic == c)
                        .ok_or_else(|| ExecError::UnknownColumn(c.clone()))
                })
                .collect::<Result<_, _>>()?;
            let rows = inner
                .rows
                .iter()
                .map(|row| indices.iter().map(|&i| row[i].clone()).collect())
                .collect();
            Ok(ResultSet {
                columns: columns.clone(),
                rows,
                ..Default::default()
            })
        }
        PlanNode::Limit { n, input } => {
            let mut inner = execute_node(input, table)?;
            inner.rows.truncate(*n as usize);
            Ok(inner)
        }
        PlanNode::Sort {
            columns: sort_cols,
            input,
        } => {
            let mut inner = execute_node(input, table)?;
            let sort_indices: Vec<(usize, bool)> = sort_cols
                .iter()
                .map(|ob| {
                    let idx = inner
                        .columns
                        .iter()
                        .position(|c| c == &ob.column)
                        .ok_or_else(|| ExecError::UnknownColumn(ob.column.clone()))?;
                    Ok((idx, ob.descending))
                })
                .collect::<Result<_, _>>()?;
            inner.rows.sort_by(|a, b| {
                for &(idx, desc) in &sort_indices {
                    let ord = a[idx].cmp(&b[idx]);
                    let ord = if desc { ord.reverse() } else { ord };
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
            Ok(inner)
        }
        PlanNode::Aggregate {
            group_by,
            aggregates,
            having,
            input,
        } => {
            let inner = execute_node(input, table)?;
            let mut result = aggregate_table(&inner.columns, &inner.rows, group_by, aggregates)?;
            if let Some(hv) = having {
                result
                    .rows
                    .retain(|row| eval_expr(hv, &result.columns, row).unwrap_or(false));
            }
            Ok(result)
        }
        PlanNode::SubqueryScan { plan, alias } => {
            let inner = execute(plan, table)?;
            let columns = inner
                .columns
                .iter()
                .map(|c| alloc::format!("{alias}.{c}"))
                .collect();
            Ok(ResultSet {
                columns,
                rows: inner.rows,
                ..Default::default()
            })
        }
    }
}
