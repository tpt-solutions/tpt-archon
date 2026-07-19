//! A persistent relational [`Database`] wired to `tpt-archon-core`.
//!
//! This is the section 4.3 integration: instead of keeping rows only in an
//! in-memory [`Table`](crate::executor::Table), the engine now stores every row
//! in a [`BTree`](tpt_archon_core::btree::BTree) from `tpt-archon-core` (which
//! sits on the unified page cache / `StorageEngine`). `INSERT` / `UPDATE` /
//! `DELETE` mutate the index; `SELECT` scans it, so the full storage stack is
//! exercised end-to-end rather than only the in-memory path.
//!
//! Row encoding is a tiny, allocation-light tag-length-value codec — no serde,
//! consistent with the zero-alloc primitives in `tpt-archon-core`.

use alloc::string::String;
use alloc::vec::Vec;

use tpt_archon_core::btree::BTree;

use crate::executor::{self, Value};
use crate::parser::{
    DeleteStatement, InsertStatement, OrderByCosine, SelectStatement, Statement, UpdateStatement,
};
use crate::planner::{plan_select, TableStats};

/// A column's logical type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    /// 64-bit integer.
    Int,
    /// UTF-8 text.
    Text,
    /// Fixed-width `f32` embedding vector (`f32[]`).
    Vector,
}

/// A table schema: ordered column names and their types.
#[derive(Debug, Clone)]
pub struct Schema {
    /// Column names in order.
    pub columns: Vec<String>,
    /// Column types, positionally aligned with `columns`.
    pub types: Vec<ColumnType>,
}

impl Schema {
    /// Looks up a column index by name.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }
}

/// Errors from executing a statement against a [`Database`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// A referenced column does not exist in the schema.
    UnknownColumn(String),
    /// A `WHERE` predicate compared against a non-integer column.
    TypeMismatch,
    /// A value literal did not match the column's declared type.
    ColumnTypeMismatch(String),
    /// A `VALUES` list had a different arity than the column list.
    ArityMismatch,
    /// `ORDER BY cosine(col, ?)` referenced a column that is not a vector.
    NotAVectorColumn(String),
    /// A `?` query parameter was expected but not supplied.
    MissingParam,
}

/// A small relational database backed by `tpt-archon-core`'s B-Link tree.
#[derive(Debug)]
pub struct Database {
    schema: Schema,
    tree: BTree,
    next_row_id: u64,
}

impl Database {
    /// Creates an empty database with the given schema.
    pub fn new(schema: Schema) -> Self {
        Self {
            schema,
            tree: BTree::new(),
            next_row_id: 0,
        }
    }

    /// The schema of this database.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Number of rows currently stored.
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    /// Whether the database has no rows.
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// Executes a parsed [`Statement`], returning a [`ResultSet`] for queries.
    pub fn execute(
        &mut self,
        stmt: &Statement,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        match stmt {
            Statement::Select(s) => self.run_select(s, params),
            Statement::Insert(i) => {
                self.run_insert_stmt(i)?;
                Ok(executor::ResultSet {
                    columns: Vec::new(),
                    rows: Vec::new(),
                })
            }
            Statement::Update(u) => {
                self.run_update(u)?;
                Ok(executor::ResultSet {
                    columns: Vec::new(),
                    rows: Vec::new(),
                })
            }
            Statement::Delete(d) => {
                self.run_delete(d)?;
                Ok(executor::ResultSet {
                    columns: Vec::new(),
                    rows: Vec::new(),
                })
            }
        }
    }

    /// Like [`execute`](Database::execute) but takes ownership of the statement,
    /// used by callers that build statements directly (e.g. arity tests).
    pub fn execute_checked(&mut self, stmt: &Statement) -> Result<executor::ResultSet, DbError> {
        self.execute(stmt, &[])
    }

    fn run_insert_stmt(&mut self, stmt: &InsertStatement) -> Result<(), DbError> {
        let cols: Vec<usize> = if stmt.columns.is_empty() {
            (0..self.schema.columns.len()).collect()
        } else {
            stmt.columns
                .iter()
                .map(|c| {
                    self.schema
                        .index_of(c)
                        .ok_or_else(|| DbError::UnknownColumn(c.clone()))
                })
                .collect::<Result<_, _>>()?
        };
        if stmt.values.len() != cols.len() {
            return Err(DbError::ArityMismatch);
        }
        let mut row = vec![Value::Int(0); self.schema.columns.len()];
        for (slot, lit) in cols.iter().zip(stmt.values.iter()) {
            row[*slot] = self.literal_to_value(*slot, lit)?;
        }
        let id = self.next_row_id;
        self.next_row_id += 1;
        row[0] = Value::Int(id as i64); // implicit row_id slot
        self.run_insert(id, row);
        Ok(())
    }

    // --- row codec ---------------------------------------------------------

    fn encode_row(&self, values: &[Value]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(values.len() as u16).to_le_bytes());
        for v in values {
            match v {
                Value::Int(i) => {
                    out.push(0);
                    out.extend_from_slice(&i.to_le_bytes());
                }
                Value::Text(t) => {
                    out.push(1);
                    out.extend_from_slice(&(t.len() as u32).to_le_bytes());
                    out.extend_from_slice(t.as_bytes());
                }
                Value::Vector(vec) => {
                    out.push(2);
                    out.extend_from_slice(&(vec.len() as u32).to_le_bytes());
                    for f in vec {
                        out.extend_from_slice(&f.to_le_bytes());
                    }
                }
            }
        }
        out
    }

    fn decode_row(&self, bytes: &[u8]) -> Vec<Value> {
        let mut pos = 0usize;
        let n = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        let mut row = Vec::with_capacity(n);
        for _ in 0..n {
            let tag = bytes[pos];
            pos += 1;
            match tag {
                0 => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&bytes[pos..pos + 8]);
                    pos += 8;
                    row.push(Value::Int(i64::from_le_bytes(b)));
                }
                1 => {
                    let len = u32::from_le_bytes([
                        bytes[pos],
                        bytes[pos + 1],
                        bytes[pos + 2],
                        bytes[pos + 3],
                    ]) as usize;
                    pos += 4;
                    let s = String::from_utf8_lossy(&bytes[pos..pos + len]).into_owned();
                    pos += len;
                    row.push(Value::Text(s));
                }
                2 => {
                    let len = u32::from_le_bytes([
                        bytes[pos],
                        bytes[pos + 1],
                        bytes[pos + 2],
                        bytes[pos + 3],
                    ]) as usize;
                    pos += 4;
                    let mut vec = Vec::with_capacity(len);
                    for _ in 0..len {
                        let mut b = [0u8; 4];
                        b.copy_from_slice(&bytes[pos..pos + 4]);
                        pos += 4;
                        vec.push(f32::from_le_bytes(b));
                    }
                    row.push(Value::Vector(vec));
                }
                _ => row.push(Value::Int(0)),
            }
        }
        row
    }

    // --- DML ---------------------------------------------------------------

    fn run_insert(&mut self, id: u64, values: Vec<Value>) {
        self.tree.insert(id, self.encode_row(&values));
    }

    fn run_update(&mut self, stmt: &UpdateStatement) -> Result<(), DbError> {
        let matching: Vec<u64> = self.matching_row_ids(stmt.filter.as_ref())?;
        for id in matching {
            let bytes = self.tree.get(id).unwrap().to_vec();
            let mut row = self.decode_row(&bytes);
            for a in &stmt.assignments {
                let slot = self
                    .schema
                    .index_of(&a.column)
                    .ok_or_else(|| DbError::UnknownColumn(a.column.clone()))?;
                if slot == 0 {
                    continue; // never overwrite the implicit row_id slot
                }
                row[slot] = self.literal_to_value(slot, &a.value)?;
            }
            self.tree.insert(id, self.encode_row(&row));
        }
        Ok(())
    }

    fn run_delete(&mut self, stmt: &DeleteStatement) -> Result<(), DbError> {
        let matching = self.matching_row_ids(stmt.filter.as_ref())?;
        for id in matching {
            self.tree.delete(id);
        }
        Ok(())
    }

    /// Returns the row ids whose `Int` column satisfies the predicate. A `None`
    /// predicate matches every row.
    fn matching_row_ids(&self, pred: Option<&crate::parser::Predicate>) -> Result<Vec<u64>, DbError> {
        let mut out = Vec::new();
        let mut id = 0u64;
        while let Some(bytes) = self.tree.get(id) {
            let row = self.decode_row(bytes);
            let keep = match pred {
                None => true,
                Some(p) => {
                    let slot = self
                        .schema
                        .index_of(&p.column)
                        .ok_or_else(|| DbError::UnknownColumn(p.column.clone()))?;
                    match &row[slot] {
                        Value::Int(v) => executor::cmp_matches_pub(p.op, *v, p.value),
                        _ => return Err(DbError::TypeMismatch),
                    }
                }
            };
            if keep {
                out.push(id);
            }
            id += 1;
        }
        Ok(out)
    }

    // --- SELECT ------------------------------------------------------------

    fn run_select(&self, stmt: &SelectStatement, params: &[Vec<f32>]) -> Result<executor::ResultSet, DbError> {
        if let Some(ob) = &stmt.order_by_cosine {
            return self.run_vector_topk(stmt, ob, params);
        }

        let mut table = executor::Table::new(self.schema.columns.clone());
        let mut id = 0u64;
        while let Some(bytes) = self.tree.get(id) {
            let row = self.decode_row(bytes);
            let keep = match &stmt.filter {
                None => true,
                Some(p) => {
                    let slot = self
                        .schema
                        .index_of(&p.column)
                        .ok_or_else(|| DbError::UnknownColumn(p.column.clone()))?;
                    match &row[slot] {
                        Value::Int(v) => executor::cmp_matches_pub(p.op, *v, p.value),
                        _ => return Err(DbError::TypeMismatch),
                    }
                }
            };
            if keep {
                table.insert(row);
            }
            id += 1;
        }

        let plan = plan_select(
            stmt,
            TableStats {
                row_count: table.rows.len() as u64,
            },
        );
        executor::execute(&plan, &table).map_err(|e| match e {
            executor::ExecError::UnknownColumn(c) => DbError::UnknownColumn(c),
            executor::ExecError::TypeMismatch => DbError::TypeMismatch,
        })
    }

    fn run_vector_topk(
        &self,
        stmt: &SelectStatement,
        ob: &OrderByCosine,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        let slot = self
            .schema
            .index_of(&ob.column)
            .ok_or_else(|| DbError::UnknownColumn(ob.column.clone()))?;
        if self.schema.types[slot] != ColumnType::Vector {
            return Err(DbError::NotAVectorColumn(ob.column.clone()));
        }
        let mut embeddings: Vec<Vec<f32>> = Vec::new();
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut id = 0u64;
        while let Some(bytes) = self.tree.get(id) {
            let row = self.decode_row(bytes);
            if let Value::Vector(v) = &row[slot] {
                embeddings.push(v.clone());
                rows.push(row);
            }
            id += 1;
        }
        let query = params.get(ob.param - 1).ok_or(DbError::MissingParam)?;
        let top = executor::vector_topk(&embeddings, query, ob.k as usize);
        let mut out_rows = Vec::new();
        for &i in &top {
            out_rows.push(rows[i].clone());
        }
        let columns = if stmt.star || stmt.columns.is_empty() {
            self.schema.columns.clone()
        } else {
            stmt.columns.clone()
        };
        Ok(executor::ResultSet {
            columns,
            rows: out_rows,
        })
    }

    fn literal_to_value(&self, slot: usize, lit: &crate::parser::Literal) -> Result<Value, DbError> {
        let expected = &self.schema.types[slot];
        match (expected, lit) {
            (ColumnType::Int, crate::parser::Literal::Int(i)) => Ok(Value::Int(*i)),
            (ColumnType::Text, crate::parser::Literal::Text(t)) => Ok(Value::Text(t.clone())),
            (ColumnType::Vector, crate::parser::Literal::Vector(v)) => Ok(Value::Vector(v.clone())),
            (ColumnType::Int, _)
            | (ColumnType::Text, _)
            | (ColumnType::Vector, _) => {
                Err(DbError::ColumnTypeMismatch(self.schema.columns[slot].clone()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_statement;
    use crate::parser::{InsertStatement, Literal};

    fn schema() -> Schema {
        Schema {
            columns: alloc::vec!["id".to_string(), "name".to_string(), "age".to_string()],
            types: alloc::vec![ColumnType::Int, ColumnType::Text, ColumnType::Int],
        }
    }

    fn db() -> Database {
        Database::new(schema())
    }

    #[test]
    fn execute_dispatch_insert_select_update_delete() {
        let mut d = db();
        d.execute(
            &parse_statement("INSERT INTO users (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        assert_eq!(d.len(), 1);

        let r = d
            .execute(
                &parse_statement("SELECT id, name FROM users WHERE age >= 30").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(1));

        d.execute(&parse_statement("UPDATE users SET age = 99 WHERE age < 50").unwrap(), &[])
            .unwrap();
        let r2 = d
            .execute(&parse_statement("SELECT id FROM users WHERE age = 99").unwrap(), &[])
            .unwrap();
        assert_eq!(r2.rows.len(), 1);

        d.execute(&parse_statement("DELETE FROM users WHERE age = 99").unwrap(), &[])
            .unwrap();
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn arity_and_type_errors() {
        let mut d = db();
        // Build an INSERT with an arity mismatch and exercise the checked path.
        let ins = InsertStatement {
            table: "users".to_string(),
            columns: alloc::vec!["id".to_string()],
            values: alloc::vec![Literal::Int(1), Literal::Int(2)],
        };
        assert!(matches!(
            d.execute_checked(&Statement::Insert(ins)),
            Err(DbError::ArityMismatch)
        ));

        let bad_ty = parse_statement("INSERT INTO users (id, name, age) VALUES (1, 5, 30)").unwrap();
        assert_eq!(
            d.execute(&bad_ty, &[]),
            Err(DbError::ColumnTypeMismatch("name".to_string()))
        );
    }

    #[test]
    fn vector_topk_query() {
        let schema = Schema {
            columns: alloc::vec!["id".to_string(), "emb".to_string()],
            types: alloc::vec![ColumnType::Int, ColumnType::Vector],
        };
        let mut d = Database::new(schema);
        let rows = ["[1.0, 0.0]", "[0.0, 1.0]", "[0.9, 0.1]"];
        for (i, emb) in rows.iter().enumerate() {
            let sql = alloc::format!("INSERT INTO docs (id, emb) VALUES ({i}, {emb})");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let sel = parse_statement("SELECT id FROM docs ORDER BY cosine(emb, ?) LIMIT 2").unwrap();
        let r = d.execute(&sel, &[alloc::vec![1.0, 0.0]]).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], Value::Int(0));
        assert_eq!(r.rows[1][0], Value::Int(2));
    }
}
