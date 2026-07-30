//! The executor's row/table data model: [`Value`], [`Row`], [`Table`], and
//! the [`ExecError`]/[`ResultSet`] types every execution path produces or
//! consumes, plus [`literal_to_value`] for lifting a parsed literal into one.

use alloc::string::String;
use alloc::vec::Vec;

use crate::parser::Literal;

/// A single value in a row (integers, text, float, an embedding vector, or NULL).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A 64-bit integer.
    Int(i64),
    /// A 32-bit floating-point number.
    Float(f32),
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
            (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (Value::Vector(a), Value::Vector(b)) => a.len().cmp(&b.len()).then_with(|| {
                a.iter()
                    .zip(b.iter())
                    .map(|(x, y)| x.to_bits().cmp(&y.to_bits()))
                    .find(|o| *o != core::cmp::Ordering::Equal)
                    .unwrap_or(core::cmp::Ordering::Equal)
            }),
            (Value::Null, Value::Null) => core::cmp::Ordering::Equal,
            (Value::Null, _) => core::cmp::Ordering::Greater,
            (_, Value::Null) => core::cmp::Ordering::Less,
            (Value::Int(_), _) => core::cmp::Ordering::Less,
            (_, Value::Int(_)) => core::cmp::Ordering::Greater,
            (Value::Float(_), Value::Text(_)) | (Value::Float(_), Value::Vector(_)) => core::cmp::Ordering::Less,
            (Value::Text(_), Value::Float(_))
            | (Value::Vector(_), Value::Float(_)) => core::cmp::Ordering::Greater,
            (Value::Text(_), Value::Vector(_)) => core::cmp::Ordering::Less,
            (Value::Vector(_), Value::Text(_)) => core::cmp::Ordering::Greater,
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

/// Converts a parser-level [`Literal`] into a runtime [`Value`].
pub fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Int(v) => Value::Int(*v),
        Literal::Float(v) => Value::Float(*v),
        Literal::Text(s) => Value::Text(s.clone()),
        Literal::Vector(v) => Value::Vector(v.clone()),
        Literal::Null => Value::Null,
    }
}
