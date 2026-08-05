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

use crate::parser::{FrameBound, FrameKind, WindowCall, WindowFrame, WindowFunc};

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
                // Postgres's default frame is RANGE UNBOUNDED PRECEDING ..
                // CURRENT ROW when an ORDER BY is present, which groups tied
                // ORDER BY values into a single peer (unlike a ROWS frame,
                // which would treat each physical row independently and
                // silently mis-sum ties).
                let default_frame = if call.spec.order_by.is_empty() {
                    WindowFrame {
                        kind: FrameKind::Rows,
                        start: FrameBound::UnboundedPreceding,
                        end: FrameBound::UnboundedFollowing,
                    }
                } else {
                    WindowFrame {
                        kind: FrameKind::Range,
                        start: FrameBound::UnboundedPreceding,
                        end: FrameBound::CurrentRow,
                    }
                };
                let frame = call.spec.frame.unwrap_or(default_frame);
                // A RANGE frame with a numeric offset needs exactly one ORDER BY
                // column (value arithmetic); the default peer-group frame only
                // needs equality across the ORDER BY columns.
                if frame.kind == FrameKind::Range {
                    let uses_numeric = matches!(
                        frame.start,
                        FrameBound::Preceding(_) | FrameBound::Following(_)
                    ) || matches!(frame.end, FrameBound::Preceding(_) | FrameBound::Following(_));
                    if uses_numeric && order_indices.len() != 1 {
                        return Err(ExecError::Unsupported(
                            "RANGE frame with a numeric offset requires exactly one ORDER BY column"
                                .to_string(),
                        ));
                    }
                }
                // Precompute the single ORDER BY column's values for this
                // partition (sorted, ascending) when a RANGE value-based frame
                // needs them.
                let order_vals: Vec<Value> =
                    if frame.kind == FrameKind::Range && order_indices.len() == 1 {
                        let oi = order_indices[0].0;
                        part_rows.iter().map(|&i| rows[i][oi].clone()).collect()
                    } else {
                        Vec::new()
                    };
                for pos in 0..m {
                    let (start, end) =
                        resolve_frame(frame, pos, m, &order_vals, &order_indices, rows, part_rows);
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

/// Resolves a [`WindowFrame`] to a concrete `[start, end]` (inclusive) physical
/// row range within a partition of size `m`, given the current row's sorted
/// position `pos` and the partition's (already-sorted) row indices.
///
/// `Rows` frames count physical rows (via [`resolve_bound`]); `Range` frames
/// count in `ORDER BY` value space. For a single `ORDER BY` column this is done
/// by value comparison (supporting numeric offsets and the default peer-group
/// frame); for multiple `ORDER BY` columns the default `RANGE` frame groups
/// rows that tie on *all* order columns into one peer.
fn resolve_frame(
    frame: WindowFrame,
    pos: usize,
    m: usize,
    order_vals: &[Value],
    order_indices: &[(usize, bool)],
    rows: &[Row],
    part_rows: &[usize],
) -> (usize, usize) {
    match frame.kind {
        FrameKind::Rows => (resolve_bound(frame.start, pos, m), resolve_bound(frame.end, pos, m)),
        FrameKind::Range => {
            if order_indices.len() > 1 {
                // Peer group across all ORDER BY columns (default RANGE frame).
                let ps = {
                    let mut p = pos;
                    while p > 0 && peers_equal(rows, part_rows, order_indices, p - 1, pos) {
                        p -= 1;
                    }
                    p
                };
                let pe = {
                    let mut p = pos;
                    while p + 1 < m && peers_equal(rows, part_rows, order_indices, p + 1, pos) {
                        p += 1;
                    }
                    p
                };
                let s = match frame.start {
                    FrameBound::UnboundedPreceding => 0,
                    FrameBound::CurrentRow => ps,
                    _ => 0,
                };
                let e = match frame.end {
                    FrameBound::UnboundedFollowing => m.saturating_sub(1),
                    FrameBound::CurrentRow => pe,
                    _ => m.saturating_sub(1),
                };
                (s, e)
            } else {
                resolve_range_by_value(order_vals, pos, m, frame.start, frame.end)
            }
        }
    }
}

/// Resolves a `Range` frame over a single (sorted) `ORDER BY` column by value.
/// Handles both the default peer-group frame (`CURRENT ROW` end → rows whose
/// value equals the current row's) and numeric offsets (`k PRECEDING`/`k
/// FOLLOWING` → rows whose value lies within `[cur - k, cur + k]` as
/// appropriate). Direction of the ORDER BY does not matter — comparison is on
/// the values themselves.
fn resolve_range_by_value(
    order_vals: &[Value],
    pos: usize,
    m: usize,
    start: FrameBound,
    end: FrameBound,
) -> (usize, usize) {
    let cur = &order_vals[pos];
    let mut low: Option<Value> = None;
    let mut high: Option<Value> = None;
    match start {
        FrameBound::UnboundedPreceding => {}
        FrameBound::UnboundedFollowing => {}
        FrameBound::CurrentRow => {
            low = Some(cur.clone());
            high = Some(cur.clone());
        }
        FrameBound::Preceding(k) => low = apply_offset(cur, k, false),
        FrameBound::Following(k) => low = apply_offset(cur, k, true),
    }
    match end {
        FrameBound::UnboundedFollowing => {}
        FrameBound::UnboundedPreceding => {}
        FrameBound::CurrentRow => {
            low = combine_low(low, Some(cur.clone()));
            high = combine_high(high, Some(cur.clone()));
        }
        FrameBound::Preceding(k) => high = combine_high(high, apply_offset(cur, k, false)),
        FrameBound::Following(k) => high = combine_high(high, apply_offset(cur, k, true)),
    }
    let mut s = m;
    let mut e = 0;
    let mut any = false;
    for (i, v) in order_vals.iter().enumerate() {
        let ge_low = match &low {
            None => true,
            Some(l) => v.cmp(l) != core::cmp::Ordering::Less,
        };
        let le_high = match &high {
            None => true,
            Some(h) => v.cmp(h) != core::cmp::Ordering::Greater,
        };
        if ge_low && le_high {
            any = true;
            if i < s {
                s = i;
            }
            if i > e {
                e = i;
            }
        }
    }
    if !any {
        s = 1;
        e = 0;
    }
    (s, e)
}

/// True when the rows at sorted positions `a` and `b` tie on every `ORDER BY`
/// column (used for multi-column `RANGE` peer grouping).
fn peers_equal(
    rows: &[Row],
    part_rows: &[usize],
    order_indices: &[(usize, bool)],
    a: usize,
    b: usize,
) -> bool {
    let ra = &rows[part_rows[a]];
    let rb = &rows[part_rows[b]];
    order_indices.iter().all(|&(oi, _)| ra[oi] == rb[oi])
}

/// Adds (`following`) or subtracts (`!following`) `k` from a numeric value.
/// Returns `None` for non-numeric values (callers reject numeric-offset
/// `RANGE` frames on non-numeric columns before reaching here).
fn apply_offset(v: &Value, k: u64, following: bool) -> Option<Value> {
    match v {
        Value::Int(n) => {
            let kk = k as i64;
            let r = if following {
                n.checked_add(kk)
            } else {
                n.checked_sub(kk)
            };
            r.map(Value::Int)
        }
        Value::Float(f) => {
            let r = if following {
                *f + k as f32
            } else {
                *f - k as f32
            };
            Some(Value::Float(r))
        }
        _ => None,
    }
}

/// Takes the more restrictive (greater) of two lower bounds; `None` is
/// unbounded (the least restrictive).
fn combine_low(a: Option<Value>, b: Option<Value>) -> Option<Value> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if x.cmp(&y) == core::cmp::Ordering::Greater {
            x
        } else {
            y
        }),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Takes the more restrictive (lesser) of two upper bounds; `None` is
/// unbounded (the least restrictive).
fn combine_high(a: Option<Value>, b: Option<Value>) -> Option<Value> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if x.cmp(&y) == core::cmp::Ordering::Less {
            x
        } else {
            y
        }),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}
