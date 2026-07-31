//! Node.js bindings for the `tpt-archon-relational` embeddable SQL engine,
//! via [napi-rs](https://napi.rs).
//!
//! This is a thin wrapper: SQL text goes in via [`Database::execute`],
//! plain JS objects (`Record<string, any>[]`) come out. It does not invent a
//! new query surface -- the SQL flow it exposes is the same one shown in the
//! root `README.md`'s Quick Start (`CREATE TABLE`, `INSERT`, `SELECT ...
//! WHERE ... ORDER BY`), plus the vector-search path
//! (`ORDER BY cosine(col, ?) LIMIT k`) that's the differentiating feature
//! worth demoing from Node for RAG-style workloads.
//!
//! See `crates/out-archon-node/README.md` for build/usage instructions and
//! an honest list of what is and isn't wired up yet.

#![deny(clippy::all)]

use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::{Map, Number, Value as JsonValue};

use tpt_archon_relational::database::{Database as CoreDatabase, DbError};
use tpt_archon_relational::executor::Value as CoreValue;
use tpt_archon_relational::parser::parse_statement;

/// An embedded Archon SQL database.
///
/// Wraps [`tpt_archon_relational::database::Database`]: an in-process
/// relational engine over `tpt-archon-core`'s B-Link tree (multiple tables,
/// `WHERE`/`JOIN`/`GROUP BY`/`ORDER BY`, `BEGIN`/`COMMIT`/`ROLLBACK` MVCC
/// transactions, and a vector `f32[]` column type with
/// `ORDER BY cosine(col, ?) LIMIT k` top-k search).
///
/// There is no separate "open file" constructor yet -- every `Database` is
/// in-memory for the lifetime of the Node process. See the crate README for
/// the full list of what this does and doesn't do yet.
#[napi]
pub struct Database {
    inner: CoreDatabase,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

#[napi]
impl Database {
    /// Creates a new, empty database with no tables.
    ///
    /// There is intentionally no schema-object constructor: the underlying
    /// Rust API's own docs mark `Database::new(schema)` "legacy" and
    /// recommend `Database::empty()` plus `CREATE TABLE` instead, so that's
    /// what this binding exposes too -- define tables by executing
    /// `CREATE TABLE` statements through [`Database::execute`].
    #[napi(constructor)]
    pub fn new() -> Self {
        Database {
            inner: CoreDatabase::empty(),
        }
    }

    /// Executes one SQL statement and returns its result rows as plain JS
    /// objects, one per row, keyed by column name.
    ///
    /// `vectorParams`, if given, supplies the query vector(s) for
    /// `ORDER BY cosine(col, ?) LIMIT k` -- the `?` there is a vector
    /// placeholder, not a general parameter-binding mechanism for other
    /// literal types. This mirrors
    /// `tpt_archon_relational::database::Database::execute`'s
    /// `params: &[Vec<f32>]` argument exactly. Every other literal (ints,
    /// text, and vector literals like `[0.1, 0.2, 0.3]`) is written directly
    /// into the SQL text, the same way `archon-sql` and the Rust examples do.
    ///
    /// Statements that don't produce rows (`CREATE TABLE`, `INSERT`,
    /// `UPDATE`, `DELETE`, `BEGIN`/`COMMIT`/`ROLLBACK`, `CREATE`/`DROP VIEW`,
    /// `ALTER TABLE`) return `[]`.
    ///
    /// Throws a JS `Error` on a SQL parse error or a database error (e.g.
    /// unknown table/column, arity mismatch, transaction conflict).
    #[napi]
    pub fn execute(
        &mut self,
        sql: String,
        vector_params: Option<Vec<Vec<f64>>>,
    ) -> Result<JsonValue> {
        let stmt = parse_statement(&sql)
            .map_err(|e| Error::from_reason(format!("parse error: {}", e.0)))?;

        let params: Vec<Vec<f32>> = vector_params
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.into_iter().map(|f| f as f32).collect())
            .collect();

        let rs = self
            .inner
            .execute(&stmt, &params)
            .map_err(|e| Error::from_reason(fmt_db_error(&e)))?;

        let rows: Vec<JsonValue> = rs
            .rows
            .iter()
            .map(|row| {
                let mut obj = Map::new();
                for (col, val) in rs.columns.iter().zip(row.iter()) {
                    obj.insert(col.clone(), value_to_json(val));
                }
                JsonValue::Object(obj)
            })
            .collect();

        Ok(JsonValue::Array(rows))
    }

    /// Returns the names of all tables currently defined.
    #[napi]
    pub fn table_names(&self) -> Vec<String> {
        self.inner
            .table_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }
}

/// Converts one row cell to its JSON (and therefore native-JS) representation.
///
/// `Int` values round-trip through `f64` (JS's only number type), so
/// magnitudes beyond +/-2^53 lose precision -- a real limitation, not an
/// oversight; see the crate README.
fn value_to_json(v: &CoreValue) -> JsonValue {
    match v {
        CoreValue::Int(i) => JsonValue::Number(Number::from(*i)),
        CoreValue::Text(s) => JsonValue::String(s.clone()),
        CoreValue::Vector(v) => JsonValue::Array(
            v.iter()
                .map(|f| {
                    Number::from_f64(*f as f64)
                        .map(JsonValue::Number)
                        .unwrap_or(JsonValue::Null)
                })
                .collect(),
        ),
        CoreValue::Null => JsonValue::Null,
    }
}

/// Human-readable error text surfaced as a thrown JS `Error`'s message.
/// Mirrors `out-archon-sql`'s `fmt_db_error` so the same error, reported
/// through either front end, reads the same way.
fn fmt_db_error(e: &DbError) -> String {
    match e {
        DbError::UnknownColumn(c) => format!("unknown column '{c}'"),
        DbError::TypeMismatch => "type mismatch".to_string(),
        DbError::ColumnTypeMismatch(c) => format!("type mismatch for column '{c}'"),
        DbError::ArityMismatch => "arity mismatch".to_string(),
        DbError::NotAVectorColumn(c) => format!("column '{c}' is not a vector column"),
        DbError::MissingParam => "missing query parameter".to_string(),
        DbError::RowNotFound(id) => format!("row {id} not found"),
        DbError::CorruptRow(id) => format!("corrupt row at id {id}"),
        DbError::UnknownTable(t) => format!("unknown table '{t}'"),
        DbError::TransactionError(m) => format!("transaction error: {m}"),
        DbError::TableAlreadyExists(t) => format!("table '{t}' already exists"),
        DbError::ViewAlreadyExists(v) => format!("view '{v}' already exists"),
        DbError::UnknownView(v) => format!("unknown view '{v}'"),
        DbError::RecursiveView(v) => format!("view '{v}' cannot reference itself"),
        DbError::Unsupported(m) => format!("unsupported: {m}"),
        DbError::SubqueryCardinality(m) => format!("subquery error: {m}"),
        DbError::ColumnCountMismatch => {
            "each UNION/INTERSECT/EXCEPT query must have the same number of columns".to_string()
        }
        DbError::Exec(e) => format!("execution error: {e:?}"),
    }
}
