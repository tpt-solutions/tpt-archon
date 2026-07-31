//! Window function evaluation (`OVER (PARTITION BY ... ORDER BY ... [frame])`).
//!
//! Runs after `WHERE` filtering and before `GROUP BY`/aggregation, `ORDER BY`,
//! and `LIMIT` — matching where Postgres places window function evaluation in
//! the logical query pipeline. Each window call's output is computed as a
//! whole extra column appended to the table; the existing `Project` plan node
//! then picks it up by its alias like any other column, so no executor
//! changes were needed beyond this module.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::parser::{FrameBound, WindowCall, WindowFrame, WindowFunc};

use super::aggregate::eval_aggregate;
use super::value::{literal_to_value, ExecError, Row, Table, Value};

/// Computes every `(alias, WindowCall)` in `window_funcs` over `table`'s
/// current rows and appends each as a new column (named by its alias).
pub fn apply_window_funcs(
    table: &mut Table,
    window_funcs: &[(String, WindowCall)],
) -> Result<(), ExecError> {
    // Resolve every call against the table as it existed *before* any window
    // columns were added, so window calls in the same SELECT list can't
    // accidentally see each other's output (evaluation order stays
    // irrelevant, matching how Postgres evaluates all window functions in
    // one logical pass).
    let base_columns = table.columns.clone();
    let mut outputs = Vec::with_capacity(window_funcs.len());
    for (_, call) in window_funcs {
        outputs.push(eval_window_call(call, &base_columns, &table.rows)?);
    }
    for (alias, _) in window_funcs {
        table.columns.push(alias.clone());
    }
    for (row_idx, row) in table.rows.iter_mut().enumerate() {
        for output in &outputs {
            row.push(output[row_idx].clone());
        }
    }
    Ok(())
}

fn column_index(columns: &[String], name: &str) -> Result<usize, ExecError> {
    columns
        .iter()
        .position(|c| c == name)
        .ok_or_else(|| ExecError::UnknownColumn(name.into()))
}

fn eval_window_call(
    call: &WindowCall,
    columns: &[String],
    rows: &[Row],
) -> Result<Vec<Value>, ExecError> {
    let n = rows.len();
    let mut result = alloc::vec![Value::Null; n];

    let partition_indices: Vec<usize> = call
        .spec
        .partition_by
        .iter()
        .map(|c| column_index(columns, c))
        .collect::<Result<_, _>>()?;
    let order_indices: Vec<(usize, bool)> = call
        .spec
        .order_by
        .iter()
        .map(|ob| column_index(columns, &ob.column).map(|idx| (idx, ob.descending)))
        .collect::<Result<_, _>>()?;

    // Group row indices by partition key, preserving encounter order both
    // across partitions and within each partition (stable prior to the
    // ORDER BY sort below).
    let mut partitions: BTreeMap<Vec<Value>, Vec<usize>> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let key: Vec<Value> = partition_indices
            .iter()
            .map(|&pi| row[pi].clone())
            .collect();
        partitions.entry(key).or_default().push(i);
    }

    for part_rows in partitions.values_mut() {
        part_rows.sort_by(|&a, &b| {
            for &(idx, desc) in &order_indices {
                let ord = rows[a][idx].cmp(&rows[b][idx]);
                let ord = if desc { ord.reverse() } else { ord };
                if ord != core::cmp::Ordering::Equal {
                    return ord;
                }
            }
            core::cmp::Ordering::Equal
        });
        let m = part_rows.len();

        match &call.func {
            WindowFunc::RowNumber => {
                for (pos, &orig_idx) in part_rows.iter().enumerate() {
                    result[orig_idx] = Value::Int(pos as i64 + 1);
                }
            }
            WindowFunc::Rank | WindowFunc::DenseRank => {
                let dense = matches!(call.func, WindowFunc::DenseRank);
                let mut rank = 1i64;
                let mut dense_rank = 1i64;
                for pos in 0..m {
                    if pos > 0 {
                        let prev = part_rows[pos - 1];
                        let cur = part_rows[pos];
                        let tied = order_indices
                            .iter()
                            .all(|&(idx, _)| rows[prev][idx] == rows[cur][idx]);
                        if !tied {
                            rank = pos as i64 + 1;
                            dense_rank += 1;
                        }
                    }
                    result[part_rows[pos]] = Value::Int(if dense { dense_rank } else { rank });
                }
            }
            WindowFunc::Lag {
                column,
                offset,
                default,
            }
            | WindowFunc::Lead {
                column,
                offset,
                default,
            } => {
                let is_lag = matches!(call.func, WindowFunc::Lag { .. });
                let col_idx = column_index(columns, column)?;
                let default_val = default
                    .as_ref()
                    .map(literal_to_value)
                    .unwrap_or(Value::Null);
                for pos in 0..m {
                    let target = if is_lag {
                        pos as i64 - *offset
                    } else {
                        pos as i64 + *offset
                    };
                    let value = if target >= 0 && (target as usize) < m {
                        rows[part_rows[target as usize]][col_idx].clone()
                    } else {
                        default_val.clone()
                    };
                    result[part_rows[pos]] = value;
                }
            }
            WindowFunc::Agg { func, column } => {
                let default_frame = if call.spec.order_by.is_empty() {
                    WindowFrame {
                        start: FrameBound::UnboundedPreceding,
                        end: FrameBound::UnboundedFollowing,
                    }
                } else {
                    WindowFrame {
                        start: FrameBound::UnboundedPreceding,
                        end: FrameBound::CurrentRow,
                    }
                };
                let frame = call.spec.frame.unwrap_or(default_frame);
                for pos in 0..m {
                    let start = resolve_bound(frame.start, pos, m);
                    let end = resolve_bound(frame.end, pos, m);
                    let value = if m == 0 || start > end {
                        if *func == crate::parser::AggregateFunc::Count {
                            Value::Int(0)
                        } else {
                            Value::Null
                        }
                    } else {
                        let frame_rows: Vec<Row> =
                            (start..=end).map(|p| rows[part_rows[p]].clone()).collect();
                        eval_aggregate(*func, column, columns, &frame_rows)?
                    };
                    result[part_rows[pos]] = value;
                }
            }
        }
    }

    Ok(result)
}

/// Resolves a [`FrameBound`] to a concrete row position within a partition
/// of size `m`, given the current row's sorted position `pos`. Clamped to
/// `[0, m-1]` — a bound that would fall outside the partition (e.g.
/// `5 PRECEDING` on row 0) just clips to the partition's edge, matching
/// Postgres's own `ROWS` frame clamping behavior.
fn resolve_bound(bound: FrameBound, pos: usize, m: usize) -> usize {
    let last = m.saturating_sub(1);
    match bound {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::UnboundedFollowing => last,
        FrameBound::CurrentRow => pos,
        FrameBound::Preceding(k) => pos.saturating_sub(k as usize),
        FrameBound::Following(k) => (pos.saturating_add(k as usize)).min(last),
    }
}
