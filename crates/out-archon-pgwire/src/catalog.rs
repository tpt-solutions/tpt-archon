//! Read-only `pg_catalog` / `information_schema` emulation for the wire
//! protocol (Phase 12.3).
//!
//! Schema-introspecting drivers and ORMs issue a small, well-known set of
//! queries against `pg_catalog.*` (and `information_schema.*`) to discover
//! tables and columns. Archon has no system catalogs of its own, so this
//! module synthesizes rows directly from the live [`Database`]'s table list
//! and schemas.
//!
//! v1 coverage (the tables ORMs most commonly probe): `pg_namespace`,
//! `pg_class`, `pg_attribute`, `pg_type`, `pg_tables`, `pg_database`, plus
//! `information_schema.tables` and `information_schema.columns`. All are
//! read-only and return the full relation (no `WHERE` filtering) — drivers
//! filter client-side, which is the common case.
//!
//! Interception happens in `simple_query::handle_simple_query`: if a `SELECT`
//! targets one of these relations, the synthetic [`ResultSet`] is emitted
//! instead of being dispatched to the SQL executor (which would otherwise
//! error on an unknown table).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tpt_archon_relational::database::{ColumnType, Database};
use tpt_archon_relational::executor::{ResultSet, Value};
use tpt_archon_relational::parser::{SelectStatement, Statement, TableRef};

/// The PostgreSQL OID of the `public` namespace.
const PUBLIC_NAMESPACE_OID: i64 = 2200;
/// Base OID for user relations (above the system range).
const USER_REL_OID_BASE: i64 = 16384;

/// Returns `Some(result_set)` if `stmt` is a `SELECT` against a known catalog
/// relation, otherwise `None` (caller should run the statement normally).
pub fn handle_catalog(stmt: &Statement, db: &Database) -> Option<ResultSet> {
    let select = match stmt {
        Statement::Select(s) => s,
        _ => return None,
    };

    let table_name = match &select.table {
        TableRef::Named { name, .. } => normalize_catalog_name(name),
        TableRef::Subquery { .. } => return None,
    };

    let table = build_catalog_table(&table_name, db)?;
    Some(project(select, &table))
}

/// Strips a `pg_catalog.` / `information_schema.` schema qualifier.
fn normalize_catalog_name(name: &str) -> String {
    name.split('.')
        .next_back()
        .map(|s| s.to_string())
        .unwrap_or_else(|| name.to_string())
}

struct CatalogTable {
    columns: Vec<&'static str>,
    rows: Vec<Vec<Value>>,
}

/// Builds the synthetic rows for a named catalog relation.
fn build_catalog_table(name: &str, db: &Database) -> Option<CatalogTable> {
    match name {
        "pg_namespace" => Some(CatalogTable {
            columns: vec!["oid", "nspname", "nspowner"],
            rows: vec![vec![
                Value::Int(PUBLIC_NAMESPACE_OID),
                Value::Text("public".to_string()),
                Value::Int(0),
            ]],
        }),
        "pg_database" => Some(CatalogTable {
            columns: vec!["oid", "datname"],
            rows: vec![vec![
                Value::Int(USER_REL_OID_BASE),
                Value::Text("archon".to_string()),
            ]],
        }),
        "pg_tables" => {
            let mut rows = Vec::new();
            for t in db.table_names() {
                rows.push(vec![
                    Value::Text("public".to_string()),
                    Value::Text(t.to_string()),
                    Value::Text("archon".to_string()),
                ]);
            }
            Some(CatalogTable {
                columns: vec!["schemaname", "tablename", "tableowner"],
                rows,
            })
        }
        "tables" => {
            // information_schema.tables (normalized to its last segment)
            let mut rows = Vec::new();
            for t in db.table_names() {
                rows.push(vec![
                    Value::Text("public".to_string()),
                    Value::Text(t.to_string()),
                    Value::Text("BASE TABLE".to_string()),
                ]);
            }
            Some(CatalogTable {
                columns: vec!["table_schema", "table_name", "table_type"],
                rows,
            })
        }
        "columns" => {
            // information_schema.columns (normalized to its last segment)
            let mut rows = Vec::new();
            for t in db.table_names() {
                if let Some(schema) = db.table_schema(t) {
                    for col in &schema.columns {
                        let ctype = schema
                            .types
                            .get(schema.index_of(col).unwrap())
                            .copied()
                            .unwrap_or(ColumnType::Text);
                        rows.push(vec![
                            Value::Text("public".to_string()),
                            Value::Text(t.to_string()),
                            Value::Text(col.clone()),
                            Value::Text(ctype.pg_type_name().to_string()),
                        ]);
                    }
                }
            }
            Some(CatalogTable {
                columns: vec!["table_schema", "table_name", "column_name", "data_type"],
                rows,
            })
        }
        "pg_class" => {
            let mut rows = Vec::new();
            for t in db.table_names() {
                let oid = rel_oid(t);
                rows.push(vec![
                    Value::Int(oid),
                    Value::Text(t.to_string()),
                    Value::Int(PUBLIC_NAMESPACE_OID),
                    Value::Text("r".to_string()),
                    Value::Int(0),
                    Value::Bool(false),
                ]);
            }
            Some(CatalogTable {
                columns: vec![
                    "oid",
                    "relname",
                    "relnamespace",
                    "relkind",
                    "relam",
                    "relhasindex",
                ],
                rows,
            })
        }
        "pg_attribute" => {
            let mut rows = Vec::new();
            for t in db.table_names() {
                let oid = rel_oid(t);
                if let Some(schema) = db.table_schema(t) {
                    for (i, col) in schema.columns.iter().enumerate() {
                        let ctype = schema.types.get(i).copied().unwrap_or(ColumnType::Text);
                        let notnull = schema.not_null.iter().any(|c| c == col);
                        rows.push(vec![
                            Value::Int(oid),
                            Value::Text(col.clone()),
                            Value::Int(ctype.pg_type_oid() as i64),
                            Value::Int((i + 1) as i64),
                            Value::Bool(notnull),
                            Value::Bool(false),
                        ]);
                    }
                }
            }
            Some(CatalogTable {
                columns: vec![
                    "attrelid",
                    "attname",
                    "atttypid",
                    "attnum",
                    "attnotnull",
                    "attisdropped",
                ],
                rows,
            })
        }
        "pg_type" => Some(build_pg_type()),
        _ => None,
    }
}

/// `pg_type` rows for every column type Archon understands.
fn build_pg_type() -> CatalogTable {
    let types: &[ColumnType] = &[
        ColumnType::Int,
        ColumnType::Boolean,
        ColumnType::Float,
        ColumnType::Double,
        ColumnType::Numeric,
        ColumnType::Text,
        ColumnType::Varchar(1),
        ColumnType::Date,
        ColumnType::Timestamp,
        ColumnType::Vector,
    ];
    let mut seen = Vec::new();
    let mut rows = Vec::new();
    for ct in types {
        let name = ct.pg_type_name();
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        rows.push(vec![
            Value::Int(ct.pg_type_oid() as i64),
            Value::Text(name.to_string()),
            Value::Text("b".to_string()),
        ]);
    }
    CatalogTable {
        columns: vec!["oid", "typname", "typtype"],
        rows,
    }
}

/// Projects the requested columns (or all when `*` / no list) from a catalog
/// table into a [`ResultSet`].
fn project(select: &SelectStatement, table: &CatalogTable) -> ResultSet {
    let want_all = select.star || select.columns.iter().any(|c| c == "*");
    let cols: Vec<&'static str> = if want_all {
        table.columns.clone()
    } else {
        // Keep the requested order; resolve each against the table columns,
        // stripping any `table.` qualifier a driver may append.
        select
            .columns
            .iter()
            .filter_map(|c| {
                let bare = c.split('.').next_back().unwrap_or(c);
                table.columns.iter().find(|col| *col == &bare).copied()
            })
            .collect()
    };

    let idxs: Vec<usize> = cols
        .iter()
        .map(|c| table.columns.iter().position(|col| col == c).unwrap())
        .collect();

    let rows: Vec<Vec<Value>> = table
        .rows
        .iter()
        .map(|row| idxs.iter().map(|&i| row[i].clone()).collect())
        .collect();

    ResultSet {
        columns: cols.iter().map(|c| c.to_string()).collect(),
        rows,
        affected: None,
    }
}

/// Deterministic OID for a user relation name (stable across connections).
fn rel_oid(name: &str) -> i64 {
    let mut h: u64 = 1469598103934665603; // FNV-1a offset basis
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    USER_REL_OID_BASE + (h % 100000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_db() -> Database {
        let mut db = Database::empty();
        let ct = parse("CREATE TABLE users (user_id INT PRIMARY KEY, name TEXT NOT NULL)");
        db.execute(&ct, &[]).unwrap();
        db
    }

    fn parse(sql: &str) -> Statement {
        tpt_archon_relational::parser::parse_statement(sql).unwrap()
    }

    #[test]
    fn pg_tables_lists_user_table() {
        let db = make_db();
        let rs = handle_catalog(&parse("SELECT * FROM pg_tables"), &db).unwrap();
        assert_eq!(rs.columns, vec!["schemaname", "tablename", "tableowner"]);
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(
            rs.rows[0],
            vec![
                Value::Text("public".to_string()),
                Value::Text("users".to_string()),
                Value::Text("archon".to_string()),
            ]
        );
    }

    #[test]
    fn pg_attribute_reports_columns_and_types() {
        let db = make_db();
        let rs = handle_catalog(&parse("SELECT * FROM pg_attribute"), &db).unwrap();
        // The engine injects an implicit `id` row-id column, so the table has 3
        // attributes: id, user_id, name.
        assert_eq!(rs.rows.len(), 3);
        // Find the user_id row and verify its type/attnum/not-null.
        let user_id_row = rs
            .rows
            .iter()
            .find(|r| r[1] == Value::Text("user_id".to_string()))
            .expect("user_id attribute present");
        assert_eq!(user_id_row[2], Value::Int(20)); // int8 OID
        assert_eq!(user_id_row[3], Value::Int(2)); // attnum 2
        assert_eq!(user_id_row[4], Value::Bool(true)); // PRIMARY KEY => not null
    }

    #[test]
    fn information_schema_columns_projected() {
        let db = make_db();
        let rs = handle_catalog(
            &parse("SELECT column_name, data_type FROM information_schema.columns"),
            &db,
        )
        .unwrap();
        assert_eq!(rs.columns, vec!["column_name", "data_type"]);
        assert_eq!(rs.rows.len(), 3);
        let names: Vec<&Value> = rs.rows.iter().map(|r| &r[0]).collect();
        assert!(names
            .iter()
            .any(|v| **v == Value::Text("user_id".to_string())));
    }

    #[test]
    fn non_catalog_select_is_not_handled() {
        let db = make_db();
        assert!(handle_catalog(&parse("SELECT * FROM users"), &db).is_none());
    }
}
