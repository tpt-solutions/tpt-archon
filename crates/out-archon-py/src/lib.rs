//! Python bindings (PyO3) for [`tpt_archon_relational::database::Database`].
//!
//! This is a thin adoption wrapper, not a new engine: every method here
//! parses a SQL string with [`tpt_archon_relational::parser::parse_statement`]
//! and runs it through the same `Database::execute` path the `archon-sql`
//! REPL (`out-archon-sql`) and `tpt-archon-relational`'s own tests use. See
//! `crates/out-archon-py/README.md` for the Python-facing usage example and
//! an honest "what this is / is not yet" list.

// pyo3's `#[pymethods]`/`#[pymodule]` expansion generates wrapper functions
// that clippy flags as a redundant `PyErr -> PyErr` conversion — a known
// false positive with this macro (not a real no-op cast anywhere in our own
// code), so it's suppressed crate-wide rather than at each call site.
#![allow(clippy::useless_conversion)]

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use tpt_archon_relational::database::{
    ColumnType as RsColumnType, Database as RsDatabase, DbError,
};
use tpt_archon_relational::executor::Value as RsValue;
use tpt_archon_relational::parser::parse_statement;

/// An embedded Archon database, holding all tables/views in memory for the
/// lifetime of this Python object (see the crate-level docs above and the
/// README's "what this is not yet" section for the file-backed-storage
/// caveat).
#[pyclass]
struct Database {
    inner: RsDatabase,
}

#[pymethods]
impl Database {
    /// Creates an empty database with no tables. Use `execute("CREATE TABLE
    /// ...")` to add tables — there is no separate schema-object constructor
    /// path, matching `tpt_archon_relational::database::Database::empty()`.
    #[new]
    fn new() -> Self {
        Database {
            inner: RsDatabase::empty(),
        }
    }

    /// Parses and runs one SQL statement.
    ///
    /// `params` supplies the `f32` query vectors substituted for `?`
    /// placeholders in `ORDER BY cosine(col, ?) LIMIT k` (the vector top-k
    /// path) — pass e.g. `[[0.1, 0.2, 0.3]]` for a single placeholder.
    /// `CREATE TABLE` / `INSERT` / `UPDATE` / `DELETE` return an empty list;
    /// `SELECT` returns one `dict` per row, keyed by column name.
    #[pyo3(signature = (sql, params=None))]
    fn execute(
        &mut self,
        py: Python<'_>,
        sql: &str,
        params: Option<Vec<Vec<f32>>>,
    ) -> PyResult<Vec<PyObject>> {
        let stmt = parse_statement(sql).map_err(|e| PyValueError::new_err(e.0))?;
        let params = params.unwrap_or_default();
        let result_set = self
            .inner
            .execute(&stmt, &params)
            .map_err(|e| PyRuntimeError::new_err(format_db_error(&e)))?;

        let mut rows = Vec::with_capacity(result_set.rows.len());
        for row in &result_set.rows {
            let dict = PyDict::new_bound(py);
            for (col, val) in result_set.columns.iter().zip(row.iter()) {
                dict.set_item(col, value_to_py(py, val))?;
            }
            rows.push(dict.into_any().unbind());
        }
        Ok(rows)
    }

    /// Names of every table currently in the database.
    fn tables(&self) -> Vec<String> {
        self.inner
            .table_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// `(column_name, column_type)` pairs for `table`, or `None` if it
    /// doesn't exist. `column_type` is one of `"INT"`, `"TEXT"`, `"VECTOR"`.
    fn schema(&self, table: &str) -> Option<Vec<(String, &'static str)>> {
        self.inner.table_schema(table).map(|schema| {
            schema
                .columns
                .iter()
                .zip(schema.types.iter())
                .map(|(name, ty)| (name.clone(), column_type_name(ty)))
                .collect()
        })
    }
}

fn column_type_name(ty: &RsColumnType) -> &'static str {
    match ty {
        RsColumnType::Int => "INT",
        RsColumnType::Text => "TEXT",
        RsColumnType::Vector => "VECTOR",
    }
}

/// Maps one engine [`RsValue`] to the native Python object it represents:
/// `Int` -> `int`, `Text` -> `str`, `Vector` -> `list[float]`, `Null` ->
/// `None`.
fn value_to_py(py: Python<'_>, value: &RsValue) -> PyObject {
    match value {
        RsValue::Int(i) => i.into_py(py),
        RsValue::Text(s) => s.into_py(py),
        RsValue::Vector(v) => v.clone().into_py(py),
        RsValue::Null => py.None(),
    }
}

/// Renders a [`DbError`] as a human-readable message, mirroring
/// `out-archon-sql`'s `fmt_db_error` so Python users see the same wording the
/// REPL does rather than a raw `Debug` dump.
fn format_db_error(e: &DbError) -> String {
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
        DbError::Exec(e) => format!("execution error: {e:?}"),
    }
}

/// The `archon` extension module: `import archon; db = archon.Database()`.
#[pymodule]
fn archon(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Database>()?;
    Ok(())
}
