//! `WHERE`/`HAVING` expression evaluation against a single row.

use alloc::string::String;

use crate::parser::{CmpOp, DateTimeField, Expr};

use super::aggregate::agg_default_alias;
use super::value::{literal_to_value, ExecError, Value};

/// Resolves `name` to a value: an exact column match in `columns`/`row` first;
/// failing that, a self-qualified match (the segment after the last `.`, so
/// `orders.user_id` matches a bare `user_id` column); failing that, the same
/// two checks against `outer` (an enclosing query's columns/row), if given.
///
/// This is deliberately name-based, not a real per-table alias binding — the
/// engine doesn't track table aliases through query scopes today. It's
/// Walks the scope stack innermost-first: own columns → immediate outer
/// → grandparent → … until the name is found.
///
/// Column names may be table-qualified (e.g. `"t.id"`) or unqualified
/// (`"id"`). For qualified names, an exact match is tried first; if that
/// fails the qualifier is stripped and any column whose unqualified part
/// matches is accepted (for backwards compatibility with code that stores
/// unqualified names). For unqualified names, a plain match is tried; if
/// that fails, every column that contains the name after its last `.` is
/// checked (so `"id"` matches `"t.id"`).
pub(crate) fn find_value<'a>(
    name: &str,
    columns: &[String],
    row: &'a [Value],
    outer: &[(&[String], &'a [Value])],
) -> Option<&'a Value> {
    if let Some(idx) = columns.iter().position(|c| c == name) {
        return Some(&row[idx]);
    }
    if name.contains('.') {
        if columns.iter().any(|c| c == name) {
            return Some(&row[columns.iter().position(|c| c == name).unwrap()]);
        } else {
            for (ocols, orow) in outer {
                if let Some(idx) = ocols.iter().position(|c| c == name) {
                    return Some(&orow[idx]);
                }
            }
        }
        let stripped = &name[name.rfind('.')? + 1..];
        if let Some(idx) = columns
            .iter()
            .position(|c| c.rfind('.').map_or(c.as_str(), |i| &c[i + 1..]) == stripped)
        {
            return Some(&row[idx]);
        }
        for (ocols, orow) in outer {
            if let Some(idx) = ocols
                .iter()
                .position(|c| c.rfind('.').map_or(c.as_str(), |i| &c[i + 1..]) == stripped)
            {
                return Some(&orow[idx]);
            }
        }
        return None;
    }
    if let Some(idx) = columns
        .iter()
        .position(|c| c.rfind('.').map_or(c.as_str(), |i| &c[i + 1..]) == name)
    {
        return Some(&row[idx]);
    }
    for (ocols, orow) in outer {
        if let Some(idx) = ocols
            .iter()
            .position(|c| c.rfind('.').map_or(c.as_str(), |i| &c[i + 1..]) == name)
        {
            return Some(&orow[idx]);
        }
    }
    None
}

/// Evaluate an `Expr` against a row with the given column names.
pub fn eval_expr(expr: &Expr, columns: &[String], row: &[Value]) -> Result<bool, ExecError> {
    match eval_expr_scoped(expr, columns, row, &[])? {
        Some(b) => Ok(b),
        None => Ok(false),
    }
}

/// Evaluate an `Expr` against a row, with an `outer` scope stack for
/// correlated-subquery column resolution (see [`find_value`]).
///
/// Returns `Ok(Some(true))` / `Ok(Some(false))` / `Ok(None)` where `None`
/// represents SQL `NULL` in a boolean context (Kleene three-valued logic).
///
/// `Exists`/`InSubquery`/`ScalarCmp` need database access to run their inner
/// query and are never evaluated here — [`crate::database::Database`]
/// intercepts and resolves them before any leaf node reaches this function.
pub fn eval_expr_scoped(
    expr: &Expr,
    columns: &[String],
    row: &[Value],
    outer: &[(&[String], &[Value])],
) -> Result<Option<bool>, ExecError> {
    match expr {
        Expr::Cmp { column, op, value } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            let rhs = literal_to_value(value);
            match (v, &rhs) {
                (Value::Null, _) | (_, Value::Null) => Ok(None),
                (Value::Int(l), Value::Int(r)) => Ok(Some(eval_cmp(*op, *l, *r))),
                (Value::Float(l), Value::Float(r)) => Ok(Some(eval_float_cmp(*op, *l, *r))),
                (Value::Text(l), Value::Text(r)) => Ok(Some(eval_text_cmp(*op, l, r))),
                (Value::Bool(l), Value::Bool(r)) => Ok(Some(eval_cmp(*op, *l, *r))),
                _ => Err(ExecError::TypeMismatch),
            }
        }
        Expr::CmpColumn { left, op, right } => {
            let lv = find_value(left, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(left.clone()))?;
            let rv = find_value(right, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(right.clone()))?;
            match (lv, rv) {
                (Value::Null, _) | (_, Value::Null) => Ok(None),
                (Value::Int(l), Value::Int(r)) => Ok(Some(eval_cmp(*op, *l, *r))),
                (Value::Float(l), Value::Float(r)) => Ok(Some(eval_float_cmp(*op, *l, *r))),
                (Value::Text(l), Value::Text(r)) => Ok(Some(eval_text_cmp(*op, l, r))),
                (Value::Bool(l), Value::Bool(r)) => Ok(Some(eval_cmp(*op, *l, *r))),
                _ => Err(ExecError::TypeMismatch),
            }
        }
        Expr::IsNull { column, negated } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            let is_null = matches!(v, Value::Null);
            Ok(Some(is_null != *negated))
        }
        Expr::Like { column, pattern } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            match v {
                Value::Text(t) => Ok(Some(like_match(t, pattern))),
                Value::Null => Ok(None),
                _ => Ok(Some(false)),
            }
        }
        Expr::InInt { column, values } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            match v {
                Value::Int(v) => Ok(Some(values.contains(v))),
                Value::Null => Ok(None),
                _ => Ok(Some(false)),
            }
        }
        Expr::BetweenInt { column, low, high } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            match v {
                Value::Int(v) => Ok(Some(*v >= *low && *v <= *high)),
                Value::Null => Ok(None),
                _ => Ok(Some(false)),
            }
        }
        Expr::And(l, r) => {
            match (
                eval_expr_scoped(l, columns, row, outer)?,
                eval_expr_scoped(r, columns, row, outer)?,
            ) {
                // Kleene AND: false AND _ = false; _ AND false = false
                (Some(false), _) | (_, Some(false)) => Ok(Some(false)),
                // true AND true = true
                (Some(true), Some(true)) => Ok(Some(true)),
                // NULL AND true = NULL; true AND NULL = NULL; NULL AND NULL = NULL
                _ => Ok(None),
            }
        }
        Expr::Or(l, r) => {
            match (
                eval_expr_scoped(l, columns, row, outer)?,
                eval_expr_scoped(r, columns, row, outer)?,
            ) {
                // Kleene OR: true OR _ = true; _ OR true = true
                (Some(true), _) | (_, Some(true)) => Ok(Some(true)),
                // false OR false = false
                (Some(false), Some(false)) => Ok(Some(false)),
                // NULL OR false = NULL; false OR NULL = NULL; NULL OR NULL = NULL
                _ => Ok(None),
            }
        }
        Expr::Not(inner) => {
            match eval_expr_scoped(inner, columns, row, outer)? {
                Some(true) => Ok(Some(false)),
                Some(false) => Ok(Some(true)),
                None => Ok(None), // NOT NULL = NULL (Kleene)
            }
        }
        Expr::Agg { func, column } => {
            let name = agg_default_alias(*func, column);
            let v = find_value(&name, columns, row, outer).ok_or(ExecError::UnknownColumn(name))?;
            Ok(Some(!matches!(v, Value::Null)))
        }
        // Normally rewritten to `Cmp` by `resolve_having_aliases` before a
        // parsed HAVING is returned; this arm is the fallback for any
        // `AggCmp` that reaches evaluation directly (e.g. inside WHERE,
        // where aggregates aren't otherwise valid but aren't rejected either).
        Expr::AggCmp {
            func,
            column,
            op,
            value,
        } => {
            let name = agg_default_alias(*func, column);
            let v = find_value(&name, columns, row, outer).ok_or(ExecError::UnknownColumn(name))?;
            let rhs = literal_to_value(value);
            match (v, &rhs) {
                (Value::Null, _) | (_, Value::Null) => Ok(None),
                (Value::Int(l), Value::Int(r)) => Ok(Some(eval_cmp(*op, *l, *r))),
                (Value::Float(l), Value::Float(r)) => Ok(Some(eval_float_cmp(*op, *l, *r))),
                (Value::Text(l), Value::Text(r)) => Ok(Some(eval_text_cmp(*op, l, r))),
                _ => Err(ExecError::TypeMismatch),
            }
        }
        Expr::ExtractCmp {
            field,
            source,
            op,
            value,
        } => {
            let v = find_value(source, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(source.clone()))?;
            let rhs = literal_to_value(value);
            match (v, &rhs) {
                (Value::Null, _) | (_, Value::Null) => Ok(None),
                (Value::Int(n), Value::Int(r)) => {
                    let extracted = extract_datetime_field(*field, *n);
                    Ok(Some(eval_cmp(*op, extracted, *r)))
                }
                _ => Err(ExecError::TypeMismatch),
            }
        }
        Expr::Exists { .. } | Expr::InSubquery { .. } | Expr::ScalarCmp { .. } => {
            Err(ExecError::UnresolvedSubquery)
        }
    }
}

fn eval_cmp<T: PartialOrd>(op: CmpOp, lhs: T, rhs: T) -> bool {
    match op {
        CmpOp::Eq => lhs == rhs,
        CmpOp::Ne => lhs != rhs,
        CmpOp::Lt => lhs < rhs,
        CmpOp::Le => lhs <= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Ge => lhs >= rhs,
    }
}

fn eval_float_cmp(op: CmpOp, lhs: f32, rhs: f32) -> bool {
    match op {
        CmpOp::Eq => lhs == rhs,
        CmpOp::Ne => lhs != rhs,
        CmpOp::Lt => lhs < rhs,
        CmpOp::Le => lhs <= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Ge => lhs >= rhs,
    }
}

/// Evaluate a text comparison by comparing the actual string content.
fn eval_text_cmp(op: CmpOp, lhs: &str, rhs: &str) -> bool {
    eval_cmp(op, lhs, rhs)
}

/// Simple SQL `LIKE` matching: `%` matches any sequence, `_` matches one char.
fn like_match(text: &str, pattern: &str) -> bool {
    let t = text.as_bytes();
    let p = pattern.as_bytes();
    like_recurse(t, p)
}

fn like_recurse(text: &[u8], pat: &[u8]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }
    if pat[0] == b'%' {
        for i in 0..=text.len() {
            if like_recurse(&text[i..], &pat[1..]) {
                return true;
            }
        }
        false
    } else if pat[0] == b'_' {
        if text.is_empty() {
            false
        } else {
            like_recurse(&text[1..], &pat[1..])
        }
    } else {
        if text.is_empty() || text[0] != pat[0] {
            false
        } else {
            like_recurse(&text[1..], &pat[1..])
        }
    }
}

/// Evaluate a scalar expression against a row, returning `None` for NULL.
/// Currently wraps boolean `Expr` evaluation; structured to support future
/// scalar expression variants (column references, arithmetic, etc.).
pub fn eval_scalar(
    expr: &Expr,
    columns: &[String],
    row: &[Value],
) -> Result<Option<Value>, ExecError> {
    match eval_expr_scoped(expr, columns, row, &[])? {
        Some(true) => Ok(Some(Value::Int(1))),
        Some(false) => Ok(Some(Value::Int(0))),
        None => Ok(None),
    }
}

/// Microseconds in one day.
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Threshold below which an integer is treated as days-since-epoch (Date)
/// rather than microseconds-since-epoch (Timestamp).
const DAYS_LIKE_THRESHOLD: i64 = 1_000_000_000_000;

/// Extracts a date/time field from an integer value that is either
/// days since epoch (Date) or microseconds since epoch (Timestamp).
/// Returns the extracted field as an `i64`.
pub fn extract_datetime_field(field: DateTimeField, value: i64) -> i64 {
    // Heuristic: small absolute values are days; large ones are microseconds.
    let micros = if value.unsigned_abs() < DAYS_LIKE_THRESHOLD as u64 {
        value.saturating_mul(MICROS_PER_DAY)
    } else {
        value
    };
    match field {
        DateTimeField::Year | DateTimeField::Month | DateTimeField::Day => {
            let days = micros / MICROS_PER_DAY;
            let (y, m, d) = civil_from_days(days);
            match field {
                DateTimeField::Year => y,
                DateTimeField::Month => m,
                DateTimeField::Day => d,
                _ => unreachable!(),
            }
        }
        DateTimeField::Hour => {
            let total_secs = micros / 1_000_000;
            (total_secs / 3600) % 24
        }
        DateTimeField::Minute => {
            let total_secs = micros / 1_000_000;
            (total_secs / 60) % 60
        }
        DateTimeField::Second => micros / 1_000_000,
    }
}

/// Converts days since Unix epoch (1970-01-01) to (year, month, day).
/// Uses Howard Hinnant's civil_from_days algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
