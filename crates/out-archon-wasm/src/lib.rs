//! Browser WASM glue for `tpt-archon-relational`'s SQL engine.
//!
//! CI's `wasm` job (`cargo check -p tpt-archon-relational --no-default-features
//! --target wasm32-unknown-unknown` in `.github/workflows/ci.yml`) has long
//! proven the engine *compiles* to `wasm32-unknown-unknown`. This crate is
//! what actually wraps that compiled engine behind a `wasm-bindgen` API and a
//! static page (`www/index.html`) so a visitor can run SQL in a browser tab,
//! entirely client-side — no server involved.
//!
//! [`ArchonDb`] mirrors the same `Database::empty()` + `parse_statement` +
//! `execute` pattern the `archon-sql` REPL (`crates/out-archon-sql/src/main.rs`)
//! uses. Every statement the REPL accepts (`CREATE TABLE`, `INSERT`,
//! `SELECT`, ...) works here too, since both wrap the same
//! `tpt_archon_relational::database::Database`.
//!
//! **IS:** a thin bridge — parse one SQL statement, execute it against an
//! in-memory `Database` that lives for the tab's lifetime, and serialize the
//! resulting `ResultSet` (or error) to a JSON string `wasm-bindgen` hands
//! back to JS.
//! **NOT (yet):** persistence across page reloads, multiple databases per
//! page, streaming/incremental results for very large result sets, or a GPU
//! path (the `gpu` feature isn't wired up here — it needs an external TPTIR
//! backend a browser tab doesn't have).

use tpt_archon_relational::database::{Database, DbError};
use tpt_archon_relational::executor::{ResultSet, Value};
use tpt_archon_relational::parser::parse_statement;
use wasm_bindgen::prelude::*;

/// Installs a panic hook that forwards Rust panics to `console.error`
/// instead of letting them surface as an opaque "unreachable executed" trap.
/// Call once from JS right after instantiating the module (see
/// `www/index.js`), before constructing an [`ArchonDb`]. A no-op if the
/// `console_error_panic_hook` feature (on by default) is disabled.
#[wasm_bindgen]
pub fn init_panic_hook() {
    #[cfg(feature = "console_error_panic_hook")]
    console_error_panic_hook::set_once();
}

/// A SQL database that lives entirely in WASM linear memory for the
/// lifetime of the browser tab — no server, no filesystem.
///
/// Starts with no tables; define them with `CREATE TABLE` via
/// [`execute`](ArchonDb::execute), exactly like `Database::empty()` in the
/// `archon-sql` REPL.
#[wasm_bindgen]
pub struct ArchonDb {
    db: Database,
}

#[wasm_bindgen]
impl ArchonDb {
    /// Creates an empty database with no tables.
    #[wasm_bindgen(constructor)]
    pub fn new() -> ArchonDb {
        ArchonDb {
            db: Database::empty(),
        }
    }

    /// Parses and executes one SQL statement (a trailing `;` is optional).
    ///
    /// Returns a JSON string of the shape
    /// `{"columns": [...], "rows": [[...], ...]}` — populated for statements
    /// that produce a result set (`SELECT`), empty (`"columns": []`,
    /// `"rows": []`) for DDL/DML that doesn't (`CREATE TABLE`, `INSERT`,
    /// `UPDATE`, `DELETE`, ...). On a parse or execution error, returns
    /// `Err` with a human-readable message, which `wasm-bindgen` surfaces to
    /// JS as a thrown exception rather than a panic/abort.
    #[wasm_bindgen]
    pub fn execute(&mut self, sql: &str) -> Result<String, JsValue> {
        let sql = sql.trim().trim_end_matches(';').trim();
        let stmt = parse_statement(sql).map_err(|e| JsValue::from_str(&e.0))?;
        let result = self
            .db
            .execute(&stmt, &[])
            .map_err(|e| JsValue::from_str(&fmt_db_error(&e)))?;
        Ok(result_set_to_json(&result))
    }
}

impl Default for ArchonDb {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts a [`Value`] to its JSON representation. `Vector` serializes as a
/// JSON array of numbers; `Null` serializes as JSON `null`.
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Int(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::json!(f),
        Value::Text(s) => serde_json::json!(s),
        Value::Vector(vec) => serde_json::json!(vec),
        Value::Null => serde_json::Value::Null,
    }
}

/// Converts a [`ResultSet`] to the `{"columns": [...], "rows": [[...]]}` JSON
/// string the demo page (`www/index.js`) parses and renders as a table.
fn result_set_to_json(rs: &ResultSet) -> String {
    let rows: Vec<Vec<serde_json::Value>> = rs
        .rows
        .iter()
        .map(|row| row.iter().map(value_to_json).collect())
        .collect();
    let out = serde_json::json!({
        "columns": rs.columns,
        "rows": rows,
    });
    out.to_string()
}

/// Human-readable rendering of a [`DbError`], mirroring `archon-sql`'s
/// `fmt_db_error` (`crates/out-archon-sql/src/main.rs`) so error messages are
/// consistent between the REPL and the browser playground.
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
