//! A vectorized execution engine.
//!
//! Executes a [`Plan`] against an in-memory [`Table`], processing rows in
//! batches ([`BATCH_SIZE`]) rather than one at a time. This is the
//! CPU-only path; GPU offload (via `tpt-gpu-*`, behind the `gpu` feature) plugs
//! into the same [`Dispatch`] decision the planner already makes but is not
//! required — every query has a working CPU fallback.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::parser::{AggregateFunc, CmpOp, Expr, Literal};
use crate::planner::{Plan, PlanNode};

/// Rows processed per vectorized batch.
pub const BATCH_SIZE: usize = 1024;

/// A single value in a row (integers, text, an embedding vector, or NULL).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A 64-bit integer.
    Int(i64),
    /// A UTF-8 text value.
    Text(String),
    /// A fixed-width `f32` embedding vector (the `f32[]` column type).
    Vector(Vec<f32>),
    /// SQL `NULL`.
    Null,
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (Value::Vector(a), Value::Vector(b)) => {
                // Compare lexicographically by length first, then element-wise.
                a.len().cmp(&b.len()).then_with(|| {
                    a.iter()
                        .zip(b.iter())
                        .map(|(x, y)| x.to_bits().cmp(&y.to_bits()))
                        .find(|o| *o != core::cmp::Ordering::Equal)
                        .unwrap_or(core::cmp::Ordering::Equal)
                })
            }
            // Cross-variant ordering: Int < Text < Vector < Null (NULL sorts last).
            (Value::Null, Value::Null) => core::cmp::Ordering::Equal,
            (Value::Null, _) => core::cmp::Ordering::Greater,
            (_, Value::Null) => core::cmp::Ordering::Less,
            (Value::Int(_), _) => core::cmp::Ordering::Less,
            (_, Value::Int(_)) => core::cmp::Ordering::Greater,
            (Value::Text(_), _) => core::cmp::Ordering::Less,
            (_, Value::Text(_)) => core::cmp::Ordering::Greater,
        }
    }
}

/// A row: values positionally aligned with the table's column names.
pub type Row = Vec<Value>;

/// A simple in-memory table.
#[derive(Debug, Clone, Default)]
pub struct Table {
    /// Column names.
    pub columns: Vec<String>,
    /// Row data.
    pub rows: Vec<Row>,
}

impl Table {
    /// Creates a table with the given column names.
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    /// Appends a row.
    pub fn insert(&mut self, row: Row) {
        self.rows.push(row);
    }
}

/// Errors during execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// A referenced column does not exist in the table.
    UnknownColumn(String),
    /// The predicate compared against a non-integer column.
    TypeMismatch,
    /// A GROUP BY column was not found.
    GroupByColumnNotFound(String),
    /// An `Expr::Exists`/`InSubquery`/`ScalarCmp` node reached the pure
    /// evaluator. These require database access to run the inner query and
    /// must be intercepted by `database::Database::eval_where` before
    /// reaching here — this variant only guards against that invariant ever
    /// being violated.
    UnresolvedSubquery,
}

/// The result of running a query: output column names and rows.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSet {
    /// Output column names.
    pub columns: Vec<String>,
    /// Output rows.
    pub rows: Vec<Row>,
}

// ---------------------------------------------------------------------------
// Expression evaluation
// ---------------------------------------------------------------------------

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
    // 1. Exact match in local scope.
    if let Some(idx) = columns.iter().position(|c| c == name) {
        return Some(&row[idx]);
    }
    // 2. If qualified, try each scope checking for an exact qualified match
    //    on the column name (e.g. "t.id" matches column "t.id").
    //    This prevents "t.id" from accidentally matching "inner_t.id".
    if name.contains('.') {
        // Check if ANY column in the local scope has the full qualified name.
        if columns.iter().any(|c| c == name) {
            // Already handled by step 1 above.
        } else {
            // Try outer scopes with an exact qualified match first.
            for (ocols, orow) in outer {
                if let Some(idx) = ocols.iter().position(|c| c == name) {
                    return Some(&orow[idx]);
                }
            }
        }
        // Fall back: strip qualifier and try unqualified match.
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
    // 3. Unqualified name: try exact match, then suffix match.
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
    eval_expr_scoped(expr, columns, row, &[])
}

/// Evaluate an `Expr` against a row, with an `outer` scope stack for
/// correlated-subquery column resolution (see [`find_value`]).
///
/// `Exists`/`InSubquery`/`ScalarCmp` need database access to run their inner
/// query and are never evaluated here — [`crate::database::Database`]
/// intercepts and resolves them before any leaf node reaches this function.
pub fn eval_expr_scoped(
    expr: &Expr,
    columns: &[String],
    row: &[Value],
    outer: &[(&[String], &[Value])],
) -> Result<bool, ExecError> {
    match expr {
        Expr::Cmp { column, op, value } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            match (v, value) {
                (Value::Null, _) => Ok(false),
                (_, Literal::Null) => Ok(false),
                (Value::Int(v), Literal::Int(rhs)) => Ok(eval_cmp(*op, *v, *rhs)),
                (Value::Text(v), Literal::Text(rhs)) => Ok(eval_text_cmp(*op, v, rhs)),
                _ => Err(ExecError::TypeMismatch),
            }
        }
        Expr::CmpColumn { left, op, right } => {
            let lv = find_value(left, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(left.clone()))?;
            let rv = find_value(right, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(right.clone()))?;
            match (lv, rv) {
                (Value::Null, _) | (_, Value::Null) => Ok(false),
                (Value::Int(l), Value::Int(r)) => Ok(eval_cmp(*op, *l, *r)),
                (Value::Text(l), Value::Text(r)) => Ok(eval_text_cmp(*op, l, r)),
                _ => Err(ExecError::TypeMismatch),
            }
        }
        Expr::IsNull { column, negated } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            let is_null = matches!(v, Value::Null);
            Ok(is_null != *negated)
        }
        Expr::Like { column, pattern } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            match v {
                Value::Text(t) => Ok(like_match(t, pattern)),
                _ => Ok(false),
            }
        }
        Expr::InInt { column, values } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            match v {
                Value::Int(v) => Ok(values.contains(v)),
                _ => Ok(false),
            }
        }
        Expr::BetweenInt { column, low, high } => {
            let v = find_value(column, columns, row, outer)
                .ok_or_else(|| ExecError::UnknownColumn(column.clone()))?;
            match v {
                Value::Int(v) => Ok(*v >= *low && *v <= *high),
                _ => Ok(false),
            }
        }
        Expr::And(l, r) => {
            Ok(eval_expr_scoped(l, columns, row, outer)?
                && eval_expr_scoped(r, columns, row, outer)?)
        }
        Expr::Or(l, r) => {
            Ok(eval_expr_scoped(l, columns, row, outer)?
                || eval_expr_scoped(r, columns, row, outer)?)
        }
        Expr::Not(inner) => Ok(!eval_expr_scoped(inner, columns, row, outer)?),
        Expr::Agg { func, column } => {
            let name = agg_default_alias(*func, column);
            find_value(&name, columns, row, outer)
                .ok_or(ExecError::UnknownColumn(name))
                .map(|v| !matches!(v, Value::Null))
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
        // Try matching 0..text.len() characters.
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

// ---------------------------------------------------------------------------
// Aggregate evaluation
// ---------------------------------------------------------------------------

/// Resolves an [`Expr::Agg`] to its default column name (the alias generated
/// by the parser when no explicit `AS` is given). For explicit aliases, the
/// user references the alias directly in `HAVING` (e.g. `HAVING cnt > 1`).
pub(crate) fn agg_default_alias(func: AggregateFunc, column: &str) -> String {
    let tag = match func {
        AggregateFunc::Count => "count",
        AggregateFunc::Sum => "sum",
        AggregateFunc::Avg => "avg",
        AggregateFunc::Min => "min",
        AggregateFunc::Max => "max",
    };
    if column == "*" {
        tag.to_string()
    } else {
        alloc::format!("{tag}({column})")
    }
}

/// Runs `GROUP BY` + aggregate projection over an in-memory `(columns, rows)`
/// table, producing the aggregated result. Shared by the executor's own
/// [`PlanNode::Aggregate`] and [`Database::run_select`](crate::database::Database::run_select)'s
/// pre-planner aggregate step so the two paths don't duplicate this logic.
pub fn aggregate_table(
    columns: &[String],
    rows: &[Row],
    group_by: &[String],
    aggregates: &[(String, AggregateFunc, String)],
) -> Result<ResultSet, ExecError> {
    if group_by.is_empty() {
        let mut out_row = Vec::new();
        let mut out_cols = Vec::new();
        for (alias, func, col) in aggregates {
            out_row.push(eval_aggregate(*func, col, columns, rows)?);
            out_cols.push(alias.clone());
        }
        return Ok(ResultSet {
            columns: out_cols,
            rows: alloc::vec![out_row],
        });
    }

    let gb_indices: Vec<usize> = group_by
        .iter()
        .map(|c| {
            columns
                .iter()
                .position(|ic| ic == c)
                .ok_or_else(|| ExecError::UnknownColumn(c.clone()))
        })
        .collect::<Result<_, _>>()?;

    let mut groups: alloc::collections::BTreeMap<Vec<Value>, Vec<Row>> =
        alloc::collections::BTreeMap::new();
    for row in rows {
        let key: Vec<Value> = gb_indices.iter().map(|&i| row[i].clone()).collect();
        groups.entry(key).or_default().push(row.clone());
    }

    let mut out_cols = group_by.to_vec();
    for (alias, _, _) in aggregates {
        out_cols.push(alias.clone());
    }
    let mut out_rows = Vec::new();
    for group_rows in groups.values() {
        let mut out_row = Vec::new();
        for &idx in &gb_indices {
            out_row.push(group_rows[0][idx].clone());
        }
        for (_alias, func, col) in aggregates {
            out_row.push(eval_aggregate(*func, col, columns, group_rows)?);
        }
        out_rows.push(out_row);
    }
    Ok(ResultSet {
        columns: out_cols,
        rows: out_rows,
    })
}

fn eval_aggregate(
    func: AggregateFunc,
    column: &str,
    columns: &[String],
    rows: &[Row],
) -> Result<Value, ExecError> {
    if func == AggregateFunc::Count {
        return Ok(Value::Int(rows.len() as i64));
    }
    let idx = columns
        .iter()
        .position(|c| c == column)
        .ok_or_else(|| ExecError::UnknownColumn(column.to_string()))?;

    match func {
        AggregateFunc::Count => unreachable!(),
        AggregateFunc::Sum => {
            let sum: i64 = rows
                .iter()
                .filter_map(|r| match &r[idx] {
                    Value::Int(v) => Some(*v),
                    _ => None,
                })
                .sum();
            Ok(Value::Int(sum))
        }
        AggregateFunc::Avg => {
            let vals: Vec<i64> = rows
                .iter()
                .filter_map(|r| match &r[idx] {
                    Value::Int(v) => Some(*v),
                    _ => None,
                })
                .collect();
            if vals.is_empty() {
                Ok(Value::Int(0))
            } else {
                let avg = vals.iter().sum::<i64>() / vals.len() as i64;
                Ok(Value::Int(avg))
            }
        }
        AggregateFunc::Min => {
            let min = rows
                .iter()
                .filter_map(|r| match &r[idx] {
                    Value::Int(v) => Some(*v),
                    _ => None,
                })
                .min();
            Ok(Value::Int(min.unwrap_or(0)))
        }
        AggregateFunc::Max => {
            let max = rows
                .iter()
                .filter_map(|r| match &r[idx] {
                    Value::Int(v) => Some(*v),
                    _ => None,
                })
                .max();
            Ok(Value::Int(max.unwrap_or(0)))
        }
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

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
                rows,
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
            // Build sort-key indices.
            let sort_indices: Vec<(usize, bool)> = sort_cols
                .iter()
                .map(|ob| {
                    let idx = inner
                        .columns
                        .iter()
                        .position(|c| c == &ob.column)
                        .unwrap_or(0);
                    (idx, ob.descending)
                })
                .collect();
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
            })
        }
    }
}

/// Cosine-style vector similarity search over stored embedding rows.
///
/// This is the CPU fallback for the RAG/embeddings use case; the `gpu` feature
/// would route the same call to `tpt-gpu-*`. Returns the row indices of the
/// `k` nearest embeddings to `query` by dot-product similarity.
pub fn vector_topk(embeddings: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = embeddings
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let dot: f32 = e.iter().zip(query).map(|(a, b)| a * b).sum();
            (i, dot)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(core::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_select;
    use crate::planner::{plan_select, TableStats};
    use alloc::string::ToString;

    fn users() -> Table {
        let mut t = Table::new(alloc::vec!["id".to_string(), "age".to_string()]);
        for i in 0..10 {
            t.insert(alloc::vec![Value::Int(i), Value::Int(i * 5)]);
        }
        t
    }

    fn run(sql: &str, table: &Table) -> ResultSet {
        let stmt = parse_select(sql).unwrap();
        let plan = plan_select(
            &stmt,
            TableStats {
                row_count: table.rows.len() as u64,
            },
        );
        execute(&plan, table).unwrap()
    }

    #[test]
    fn select_star_returns_all() {
        let t = users();
        let r = run("SELECT * FROM users", &t);
        assert_eq!(r.rows.len(), 10);
        assert_eq!(r.columns, t.columns);
    }

    #[test]
    fn filter_and_project() {
        let t = users();
        let r = run("SELECT id FROM users WHERE age >= 25", &t);
        assert_eq!(r.columns, alloc::vec!["id".to_string()]);
        assert_eq!(r.rows.len(), 5);
        assert_eq!(r.rows[0], alloc::vec![Value::Int(5)]);
    }

    #[test]
    fn limit_truncates() {
        let t = users();
        let r = run("SELECT * FROM users LIMIT 3", &t);
        assert_eq!(r.rows.len(), 3);
    }

    #[test]
    fn unknown_column_errors() {
        let t = users();
        let stmt = parse_select("SELECT nope FROM users").unwrap();
        let plan = plan_select(&stmt, TableStats { row_count: 10 });
        assert_eq!(
            execute(&plan, &t),
            Err(ExecError::UnknownColumn("nope".to_string()))
        );
    }

    #[test]
    fn vector_similarity_topk() {
        let embeddings = alloc::vec![
            alloc::vec![1.0, 0.0],
            alloc::vec![0.0, 1.0],
            alloc::vec![0.9, 0.1],
        ];
        let q = alloc::vec![1.0, 0.0];
        let top = vector_topk(&embeddings, &q, 2);
        assert_eq!(top[0], 0);
        assert_eq!(top[1], 2);
    }

    #[test]
    fn evaluates_and_or_expressions() {
        let mut t = Table::new(alloc::vec![
            "id".to_string(),
            "a".to_string(),
            "b".to_string(),
        ]);
        for i in 0..10 {
            t.insert(alloc::vec![Value::Int(i), Value::Int(i), Value::Int(i * 2),]);
        }
        let r = run("SELECT * FROM t WHERE a > 3 AND b < 12", &t);
        // a > 3 → ids 4..=9; b < 12 → b = 0,2,4,6,8,10 → ids 0..=5
        // Intersection: ids 4, 5
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn evaluates_in_list() {
        let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
        for i in 0..5 {
            t.insert(alloc::vec![Value::Int(i), Value::Int(i * 10)]);
        }
        let r = run("SELECT * FROM t WHERE id IN (1, 3)", &t);
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn evaluates_between() {
        let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
        for i in 0..10 {
            t.insert(alloc::vec![Value::Int(i), Value::Int(i)]);
        }
        let r = run("SELECT * FROM t WHERE x BETWEEN 3 AND 7", &t);
        assert_eq!(r.rows.len(), 5);
    }

    #[test]
    fn evaluates_like() {
        let mut t = Table::new(alloc::vec!["id".to_string(), "name".to_string()]);
        t.insert(alloc::vec![Value::Int(0), Value::Text("alice".to_string()),]);
        t.insert(alloc::vec![Value::Int(1), Value::Text("bob".to_string()),]);
        t.insert(alloc::vec![
            Value::Int(2),
            Value::Text("alicia".to_string()),
        ]);
        let r = run("SELECT * FROM t WHERE name LIKE 'al%'", &t);
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn sort_ascending() {
        let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
        t.insert(alloc::vec![Value::Int(2), Value::Int(20)]);
        t.insert(alloc::vec![Value::Int(0), Value::Int(0)]);
        t.insert(alloc::vec![Value::Int(1), Value::Int(10)]);
        let r = run("SELECT * FROM t ORDER BY x ASC", &t);
        assert_eq!(r.rows[0][1], Value::Int(0));
        assert_eq!(r.rows[1][1], Value::Int(10));
        assert_eq!(r.rows[2][1], Value::Int(20));
    }

    #[test]
    fn aggregate_count() {
        let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
        for i in 0..5 {
            t.insert(alloc::vec![Value::Int(i), Value::Int(i)]);
        }
        let r = run("SELECT COUNT(*) FROM t", &t);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(5));
    }

    #[test]
    fn aggregate_sum() {
        let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
        for i in 1..=4 {
            t.insert(alloc::vec![Value::Int(i), Value::Int(i)]);
        }
        let r = run("SELECT SUM(x) FROM t", &t);
        assert_eq!(r.rows[0][0], Value::Int(10));
    }

    #[test]
    fn text_comparison_compares_content_not_length() {
        // Regression test: `eval_text_cmp` used to compare `lhs.len()` against
        // the RHS integer instead of the actual string content.
        let mut t = Table::new(alloc::vec!["id".to_string(), "name".to_string()]);
        t.insert(alloc::vec![Value::Int(0), Value::Text("bob".to_string())]);
        t.insert(alloc::vec![Value::Int(1), Value::Text("amy".to_string())]);
        let r = run("SELECT id FROM t WHERE name = 'bob'", &t);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(0));
    }

    #[test]
    fn is_null_is_false_for_non_null_int() {
        // Regression test: `IS NULL` used to always evaluate true for any Int
        // column regardless of value.
        let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
        t.insert(alloc::vec![Value::Int(0), Value::Int(5)]);
        t.insert(alloc::vec![Value::Int(1), Value::Null]);
        let r = run("SELECT id FROM t WHERE x IS NULL", &t);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(1));
    }

    #[test]
    fn is_not_null_negates() {
        let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
        t.insert(alloc::vec![Value::Int(0), Value::Int(5)]);
        t.insert(alloc::vec![Value::Int(1), Value::Null]);
        let r = run("SELECT id FROM t WHERE x IS NOT NULL", &t);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(0));
    }

    #[test]
    fn not_negates_expression() {
        let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
        for i in 0..5 {
            t.insert(alloc::vec![Value::Int(i), Value::Int(i)]);
        }
        let r = run("SELECT id FROM t WHERE NOT x > 2", &t);
        assert_eq!(r.rows.len(), 3);
    }
}
