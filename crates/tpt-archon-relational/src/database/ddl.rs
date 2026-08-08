//! DDL statement execution: `CREATE TABLE`, `CREATE`/`DROP VIEW`, `ALTER TABLE`.

use alloc::string::ToString;
use alloc::vec::Vec;

use tpt_archon_core::btree::BTree;

use super::schema::ForeignKey;
use crate::executor::Value;
use crate::mvcc;
use crate::parser::{
    AlterTableOp, AlterTableStatement, ColumnConstraint, CreateTableStatement, CreateViewStatement,
    TableConstraint,
};

use super::codec::{encode_row, try_decode_row};
use super::schema::{ColumnType, DbError, Schema};
use super::storage::TableStorage;
use super::{select_references_table, Database};

impl Database {
    pub(super) fn run_create_table(&mut self, ct: &CreateTableStatement) -> Result<(), DbError> {
        if self.table(&ct.table).is_some() {
            return Err(DbError::TableAlreadyExists(ct.table.clone()));
        }
        // "id" is reserved for the implicit row-id column prepended below;
        // a user column of the same name would silently collide with it.
        if ct.columns.iter().any(|c| c.name == "id") {
            return Err(DbError::Unsupported(
                "column 'id' is reserved for the implicit row-id column".to_string(),
            ));
        }
        let mut columns = Vec::new();
        let mut types = Vec::new();
        let mut not_null = Vec::new();
        let mut primary_key: Option<Vec<String>> = None;
        let mut unique = Vec::new();
        let mut foreign_keys = Vec::new();
        let mut checks = Vec::new();

        // First column is always the implicit row_id.
        columns.push("id".to_string());
        types.push(ColumnType::Int);

        for c in &ct.columns {
            columns.push(c.name.clone());
            types.push(c.ctype);
            for cc in &c.constraints {
                match cc {
                    ColumnConstraint::NotNull => {
                        if !not_null.contains(&c.name) {
                            not_null.push(c.name.clone());
                        }
                    }
                    ColumnConstraint::PrimaryKey => {
                        if primary_key.is_some() {
                            return Err(DbError::Unsupported(
                                "multiple primary keys are not supported".to_string(),
                            ));
                        }
                        primary_key = Some(vec![c.name.clone()]);
                        if !not_null.contains(&c.name) {
                            not_null.push(c.name.clone());
                        }
                    }
                    ColumnConstraint::Unique => unique.push(vec![c.name.clone()]),
                }
            }
        }

        for tc in &ct.constraints {
            match tc {
                TableConstraint::PrimaryKey(cols) => {
                    if primary_key.is_some() {
                        return Err(DbError::Unsupported(
                            "multiple primary keys are not supported".to_string(),
                        ));
                    }
                    for col in cols {
                        if !columns.contains(col) {
                            return Err(DbError::UnknownColumn(col.clone()));
                        }
                        if !not_null.contains(col) {
                            not_null.push(col.clone());
                        }
                    }
                    primary_key = Some(cols.clone());
                }
                TableConstraint::Unique(cols) => {
                    for col in cols {
                        if !columns.contains(col) {
                            return Err(DbError::UnknownColumn(col.clone()));
                        }
                    }
                    unique.push(cols.clone());
                }
                TableConstraint::ForeignKey {
                    columns: fk_cols,
                    ref_table,
                    ref_columns,
                } => {
                    for col in fk_cols {
                        if !columns.contains(col) {
                            return Err(DbError::UnknownColumn(col.clone()));
                        }
                    }
                    foreign_keys.push(ForeignKey {
                        columns: fk_cols.clone(),
                        ref_table: ref_table.clone(),
                        ref_columns: ref_columns.clone(),
                    });
                }
                TableConstraint::Check(expr) => checks.push(expr.clone()),
            }
        }

        self.tables.push((
            ct.table.clone(),
            TableStorage {
                schema: Schema {
                    columns,
                    types,
                    not_null,
                    primary_key,
                    unique,
                    foreign_keys,
                    checks,
                },
                tree: BTree::new(),
                next_row_id: 0,
                mvcc: mvcc::MvccStore::new(),
                vector_indexes: Vec::new(),
            },
        ));
        Ok(())
    }

    pub(super) fn run_create_view(&mut self, cv: &CreateViewStatement) -> Result<(), DbError> {
        if self.table(&cv.name).is_some() || self.views.iter().any(|(n, _)| n == &cv.name) {
            return Err(DbError::ViewAlreadyExists(cv.name.clone()));
        }
        if select_references_table(&cv.query, &cv.name) {
            return Err(DbError::RecursiveView(cv.name.clone()));
        }
        self.views.push((cv.name.clone(), cv.query.clone()));
        Ok(())
    }

    pub(super) fn run_drop_view(&mut self, name: &str) -> Result<(), DbError> {
        let pos = self
            .views
            .iter()
            .position(|(n, _)| n == name)
            .ok_or_else(|| DbError::UnknownView(name.to_string()))?;
        self.views.remove(pos);
        Ok(())
    }

    pub(super) fn run_alter_table(&mut self, at: &AlterTableStatement) -> Result<(), DbError> {
        let ts = self
            .table_mut(&at.table)
            .ok_or_else(|| DbError::UnknownTable(at.table.clone()))?;

        match &at.op {
            AlterTableOp::AddColumn(col) => {
                // Reject duplicate column names.
                if ts.schema.columns.iter().any(|c| c == &col.name) {
                    return Err(DbError::Unsupported(alloc::format!(
                        "column '{}' already exists",
                        col.name
                    )));
                }
                let ctype = col.ctype;
                // Re-encode every row with the new column appended (default: Null).
                let mut rows_to_reencode = Vec::new();
                for id in 0..ts.next_row_id {
                    if let Some(bytes) = ts.tree.get(id) {
                        let mut values = try_decode_row(bytes)?;
                        values.push(Value::Null);
                        rows_to_reencode.push((id, encode_row(&values)));
                    }
                }
                for (id, encoded) in &rows_to_reencode {
                    ts.tree.insert(*id, encoded.to_vec());
                }
                ts.schema.columns.push(col.name.clone());
                ts.schema.types.push(ctype);
            }
            AlterTableOp::DropColumn(name) => {
                let idx = ts
                    .schema
                    .columns
                    .iter()
                    .position(|c| c == name)
                    .ok_or_else(|| DbError::UnknownColumn(name.clone()))?;
                // Cannot drop the implicit id column.
                if idx == 0 {
                    return Err(DbError::Unsupported(
                        "cannot drop the implicit id column".to_string(),
                    ));
                }
                // Re-encode every row without the dropped column.
                let mut rows_to_reencode = Vec::new();
                for id in 0..ts.next_row_id {
                    if let Some(bytes) = ts.tree.get(id) {
                        let mut values = try_decode_row(bytes)?;
                        values.remove(idx);
                        rows_to_reencode.push((id, encode_row(&values)));
                    }
                }
                for (id, encoded) in &rows_to_reencode {
                    ts.tree.insert(*id, encoded.to_vec());
                }
                ts.schema.columns.remove(idx);
                ts.schema.types.remove(idx);
            }
            AlterTableOp::RenameColumn { old_name, new_name } => {
                let idx = ts
                    .schema
                    .columns
                    .iter()
                    .position(|c| c == old_name)
                    .ok_or_else(|| DbError::UnknownColumn(old_name.clone()))?;
                // Cannot rename the implicit id column.
                if idx == 0 {
                    return Err(DbError::Unsupported(
                        "cannot rename the implicit id column".to_string(),
                    ));
                }
                if ts.schema.columns.iter().any(|c| c == new_name) {
                    return Err(DbError::Unsupported(alloc::format!(
                        "column '{}' already exists",
                        new_name
                    )));
                }
                // Rename is metadata-only — the TLV codec is position-based.
                ts.schema.columns[idx] = new_name.clone();
            }
        }
        Ok(())
    }
}
