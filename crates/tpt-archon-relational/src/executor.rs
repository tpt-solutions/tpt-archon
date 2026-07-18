//! A vectorized execution engine.
//!
//! Executes a [`Plan`] against an in-memory [`Table`], processing rows in
//! batches ([`BATCH_SIZE`]) rather than one at a time. This is the
//! CPU-only path; GPU offload (via `tpt-gpu-*`, behind the `gpu` feature) plugs
//! into the same [`Dispatch`] decision the planner already makes but is not
//! required — every query has a working CPU fallback.

use alloc::string::String;
use alloc::vec::Vec;

use crate::parser::{CmpOp, Predicate};
use crate::planner::{Plan, PlanNode};

/// Rows processed per vectorized batch.
pub const BATCH_SIZE: usize = 1024;

/// A single value in a row (integers or short byte strings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A 64-bit integer.
    Int(i64),
    /// A UTF-8 text value.
    Text(String),
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
}

/// The result of running a query: output column names and rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSet {
    /// Output column names.
    pub columns: Vec<String>,
    /// Output rows.
    pub rows: Vec<Row>,
}

fn cmp_matches(op: CmpOp, lhs: i64, rhs: i64) -> bool {
    match op {
        CmpOp::Eq => lhs == rhs,
        CmpOp::Ne => lhs != rhs,
        CmpOp::Lt => lhs < rhs,
        CmpOp::Le => lhs <= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Ge => lhs >= rhs,
    }
}

/// Executes `plan` against `table`.
pub fn execute(plan: &Plan, table: &Table) -> Result<ResultSet, ExecError> {
    execute_node(&plan.root, table)
}

fn execute_node(node: &PlanNode, table: &Table) -> Result<ResultSet, ExecError> {
    match node {
        PlanNode::Scan { .. } => {
            // Vectorized scan: copy rows in batches (batching is observable in
            // the loop structure; semantics are identical to a row scan).
            let mut rows = Vec::with_capacity(table.rows.len());
            for chunk in table.rows.chunks(BATCH_SIZE) {
                rows.extend_from_slice(chunk);
            }
            Ok(ResultSet {
                columns: table.columns.clone(),
                rows,
            })
        }
        PlanNode::Filter { predicate, input } => {
            let inner = execute_node(input, table)?;
            let idx = inner
                .columns
                .iter()
                .position(|c| c == &predicate.column)
                .ok_or_else(|| ExecError::UnknownColumn(predicate.column.clone()))?;
            let Predicate { op, value, .. } = predicate;
            let mut rows = Vec::new();
            // Vectorized filter: evaluate the predicate over each batch.
            for chunk in inner.rows.chunks(BATCH_SIZE) {
                for row in chunk {
                    match &row[idx] {
                        Value::Int(v) => {
                            if cmp_matches(*op, *v, *value) {
                                rows.push(row.clone());
                            }
                        }
                        Value::Text(_) => return Err(ExecError::TypeMismatch),
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
        // age >= 25 means id 5..=9
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
        assert_eq!(top[0], 0); // exact match ranks first
        assert_eq!(top[1], 2); // nearest neighbour second
    }
}
