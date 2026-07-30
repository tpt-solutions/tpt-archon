//! `SELECT` execution, including vector top-k (`ORDER BY cosine(...)`).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::executor::{self, Value};
use crate::parser::{OrderByCosine, SelectStatement, TableRef, CTE};
use crate::planner::{plan_select, TableStats};
use crate::vector_index;

use super::codec::{decode_row_validated, MVCC_TOMBSTONE};
use super::schema::{ColumnType, DbError};
use super::{select_references_table, Database};

impl Database {
    /// Runs a top-level (non-correlated) `SELECT`.
    pub(super) fn run_select(
        &self,
        stmt: &SelectStatement,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        self.run_select_scoped(stmt, params, &[], &[])
    }

    /// Runs a `SELECT`, optionally scoped inside an enclosing (`outer`) row —
    /// used to evaluate a correlated subquery once per outer row. `outer` is
    /// threaded down into this query's own `WHERE` evaluation via
    /// [`Database::eval_where`], so a subquery nested inside this one only
    /// ever sees its immediate parent's row (single level of correlation).
    ///
    /// `outer_ctes` carries CTE definitions from enclosing queries so
    /// subqueries can reference them. The subquery's own CTEs shadow any
    /// matching outer CTE names (standard SQL scoping).
    pub(super) fn run_select_scoped(
        &self,
        stmt: &SelectStatement,
        params: &[Vec<f32>],
        outer: &[(&[String], &[Value])],
        outer_ctes: &[CTE],
    ) -> Result<executor::ResultSet, DbError> {
        if let Some(ob) = &stmt.order_by_cosine {
            return self.run_vector_topk(stmt, ob, params);
        }

        // Merge outer CTEs with this statement's CTEs. Subquery's own CTEs
        // shadow outer ones (standard SQL scoping).
        let mut merged_ctes: Vec<CTE> = outer_ctes.to_vec();
        for cte in &stmt.with_ctes {
            // Remove any outer CTE with the same name (subquery shadows).
            merged_ctes.retain(|c| c.name != cte.name);
            merged_ctes.push(cte.clone());
        }

        // Validate CTEs: no duplicates, no shadowing, no self-references.
        for cte in &stmt.with_ctes {
            if self.views.iter().any(|(n, _)| n == &cte.name) {
                return Err(DbError::ViewAlreadyExists(cte.name.clone()));
            }
            if self.tables.iter().any(|(n, _)| n == &cte.name) {
                return Err(DbError::ViewAlreadyExists(cte.name.clone()));
            }
            if select_references_table(&cte.query, &cte.name) {
                return Err(DbError::RecursiveView(cte.name.clone()));
            }
        }

        // Build an in-memory table from the source table + optional JOINs.
        let (mut columns, mut rows) =
            self.resolve_table_ref_with_ctes(&stmt.table, &merged_ctes)?;

        // Process JOINs (nested-loop inner join).
        for join in &stmt.joins {
            let (join_cols, join_rows) =
                self.resolve_table_ref_with_ctes(&join.table, &merged_ctes)?;
            let left_idx = columns
                .iter()
                .position(|c| c == &join.left_col)
                .ok_or_else(|| DbError::UnknownColumn(join.left_col.clone()))?;
            let right_idx = join_cols
                .iter()
                .position(|c| c == &join.right_col)
                .ok_or_else(|| DbError::UnknownColumn(join.right_col.clone()))?;

            // Rename right columns with table prefix to avoid collisions.
            let mut new_cols = columns.clone();
            let mut new_rows = Vec::new();
            for rcol in &join_cols {
                let name = alloc::format!("{}.{}", join.table.name(), rcol);
                new_cols.push(name);
            }
            for lrow in &rows {
                for jrow in &join_rows {
                    if lrow[left_idx] == jrow[right_idx] {
                        let mut combined = lrow.clone();
                        combined.extend_from_slice(jrow);
                        new_rows.push(combined);
                    }
                }
            }
            columns = new_cols;
            rows = new_rows;
        }

        // Build a Table for the executor.
        let mut table = executor::Table::new(columns);
        for row in rows {
            table.insert(row);
        }

        // Build scope-qualified column names for correlated subquery
        // resolution. The table qualifier (alias or name) is prepended to
        // each column so that `find_value("t.id")` in an inner subquery
        // matches the correct outer scope via exact match instead of
        // accidentally suffix-matching the local unqualified "id".
        let table_qualifier = stmt.table.name();
        let scope_columns: Vec<String> = table
            .columns
            .iter()
            .map(|c| {
                if c.contains('.') {
                    c.clone()
                } else {
                    alloc::format!("{table_qualifier}.{c}")
                }
            })
            .collect();

        // Apply WHERE filter.
        if let Some(expr) = &stmt.filter {
            let subquery_cache =
                self.build_subquery_cache(expr, &table.columns, params, &merged_ctes)?;
            let mut filtered = Vec::new();
            for row in &table.rows {
                if self
                    .eval_where(
                        expr,
                        &table.columns,
                        row,
                        params,
                        outer,
                        &merged_ctes,
                        &scope_columns,
                        &subquery_cache,
                        &mut 0usize,
                    )?
                    .unwrap_or(false)
                {
                    filtered.push(row.clone());
                }
            }
            table.rows = filtered;
        }

        // Apply GROUP BY + aggregates.
        if !stmt.group_by.is_empty() || !stmt.aggregates.is_empty() {
            let rs = executor::aggregate_table(
                &table.columns,
                &table.rows,
                &stmt.group_by,
                &stmt.aggregates,
            )?;
            table = executor::Table {
                columns: rs.columns,
                rows: rs.rows,
            };
        }

        // Apply HAVING filter after aggregation.
        if let Some(hv) = &stmt.having {
            let hv_cache = self.build_subquery_cache(hv, &table.columns, params, &merged_ctes)?;
            let mut filtered = Vec::new();
            for row in &table.rows {
                if self
                    .eval_where(
                        hv,
                        &table.columns,
                        row,
                        params,
                        outer,
                        &merged_ctes,
                        &table.columns,
                        &hv_cache,
                        &mut 0usize,
                    )?
                    .unwrap_or(false)
                {
                    filtered.push(row.clone());
                }
            }
            table.rows = filtered;
        }

        let plan = {
            let mut plan_stmt = stmt.clone();
            plan_stmt.group_by.clear();
            plan_stmt.aggregates.clear();
            plan_stmt.having = None;
            // The WHERE filter was already applied above with full
            // DB-aware/correlated-subquery semantics via `eval_where`;
            // clearing it here stops `plan_select` from re-wrapping it in a
            // `PlanNode::Filter`, which would otherwise re-run it through
            // `executor::execute`'s pure (non-DB-aware) evaluator and fail on
            // any subquery node.
            plan_stmt.filter = None;
            plan_select(
                &plan_stmt,
                TableStats {
                    row_count: table.rows.len() as u64,
                },
            )
        };
        executor::execute(&plan, &table).map_err(DbError::from)
    }

    fn run_vector_topk(
        &self,
        stmt: &SelectStatement,
        ob: &OrderByCosine,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        let query = ob
            .param
            .checked_sub(1)
            .and_then(|i| params.get(i))
            .ok_or(DbError::MissingParam)?;

        // For subqueries, resolve to an in-memory table first.
        if let TableRef::Subquery { .. } = &stmt.table {
            let (columns, rows) = self.resolve_table_ref(&stmt.table)?;
            let slot = columns
                .iter()
                .position(|c| c == &ob.column)
                .ok_or_else(|| DbError::UnknownColumn(ob.column.clone()))?;
            let mut embeddings = Vec::new();
            let mut data_rows = Vec::new();
            for row in &rows {
                // Apply WHERE filter before extracting embeddings.
                if let Some(expr) = &stmt.filter {
                    if !self
                        .eval_where(
                            expr,
                            &columns,
                            row,
                            params,
                            &[],
                            &[],
                            &columns,
                            &[],
                            &mut 0usize,
                        )?
                        .unwrap_or(false)
                    {
                        continue;
                    }
                }
                if let Value::Vector(v) = &row[slot] {
                    embeddings.push(v.clone());
                    data_rows.push(row.clone());
                }
            }
            let top = executor::vector_topk(&embeddings, query, ob.k as usize);
            let out_rows: Vec<Vec<Value>> = top.into_iter().map(|i| data_rows[i].clone()).collect();
            let out_columns = if stmt.star || stmt.columns.is_empty() {
                columns
            } else {
                stmt.columns.clone()
            };
            return Ok(executor::ResultSet {
                columns: out_columns,
                rows: out_rows,
            });
        }

        let table_name = match &stmt.table {
            TableRef::Named { name, .. } => name.as_str(),
            TableRef::Subquery { .. } => unreachable!(),
        };
        let ts = self
            .table(table_name)
            .ok_or_else(|| DbError::UnknownTable(table_name.to_string()))?;
        let slot = ts
            .schema
            .index_of(&ob.column)
            .ok_or_else(|| DbError::UnknownColumn(ob.column.clone()))?;
        if ts.schema.types[slot] != ColumnType::Vector {
            return Err(DbError::NotAVectorColumn(ob.column.clone()));
        }
        let mut embeddings: Vec<Vec<f32>> = Vec::new();
        let mut rows: Vec<Vec<Value>> = Vec::new();

        if let Some((_, idx)) = ts.vector_indexes.iter().find(|(c, _)| c == &ob.column) {
            // Fast path: probe the IVFFlat index instead of scanning every
            // row. Oversample beyond `k` so a WHERE filter still has enough
            // candidates left to rank after filtering — recall stays
            // approximate either way (see `vector_index` module docs), the
            // same trade pgvector's own IVFFlat index type makes.
            let fetch_k = (ob.k as usize).saturating_mul(4).max(ob.k as usize);
            for id in idx.search(query, fetch_k, vector_index::DEFAULT_NPROBE) {
                let Some(bytes) = ts.tree.get(id) else {
                    continue;
                };
                let row = decode_row_validated(id, bytes, ts.schema.columns.len())?;
                if let Some(expr) = &stmt.filter {
                    if !self
                        .eval_where(
                            expr,
                            &ts.schema.columns,
                            &row,
                            params,
                            &[],
                            &[],
                            &ts.schema.columns,
                            &[],
                            &mut 0usize,
                        )?
                        .unwrap_or(false)
                    {
                        continue;
                    }
                }
                if let Value::Vector(v) = &row[slot] {
                    embeddings.push(v.clone());
                    rows.push(row);
                }
            }
        } else {
            // No index yet for this column (table hasn't crossed
            // `vector_index::MIN_ROWS_FOR_INDEX`, or all writes so far went
            // through a transaction not yet committed) — exact brute-force
            // scan. `0..next_row_id` (not "until the first missing id")
            // because deleted rows leave holes in the middle of the range.
            for id in 0..ts.next_row_id {
                let Some(bytes) = ts.tree.get(id) else {
                    continue;
                };
                let row = decode_row_validated(id, bytes, ts.schema.columns.len())?;
                if let Some(expr) = &stmt.filter {
                    if !self
                        .eval_where(
                            expr,
                            &ts.schema.columns,
                            &row,
                            params,
                            &[],
                            &[],
                            &ts.schema.columns,
                            &[],
                            &mut 0usize,
                        )?
                        .unwrap_or(false)
                    {
                        continue;
                    }
                }
                if let Value::Vector(v) = &row[slot] {
                    embeddings.push(v.clone());
                    rows.push(row);
                }
            }
        }
        let top = executor::vector_topk(&embeddings, query, ob.k as usize);
        let mut out_rows = Vec::new();
        for &i in &top {
            out_rows.push(rows[i].clone());
        }
        let columns = if stmt.star || stmt.columns.is_empty() {
            ts.schema.columns.clone()
        } else {
            stmt.columns.clone()
        };
        Ok(executor::ResultSet {
            columns,
            rows: out_rows,
        })
    }

    /// Resolves a [`TableRef`] to `(columns, rows)` without CTE context.
    fn resolve_table_ref(&self, r: &TableRef) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        self.resolve_table_ref_with_ctes(r, &[])
    }

    /// Resolves a [`TableRef`] to `(columns, rows)`: checks CTEs first, then
    /// views, then real tables for `Named`; executes the inner query for
    /// `Subquery`.
    fn resolve_table_ref_with_ctes(
        &self,
        r: &TableRef,
        ctes: &[CTE],
    ) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        match r {
            TableRef::Named { name, .. } => {
                if let Some(cte) = ctes.iter().find(|c| &c.name == name) {
                    let rs = self.run_select_scoped(&cte.query, &[], &[], ctes)?;
                    return Ok((rs.columns, rs.rows));
                }
                if let Some((_, query)) = self.views.iter().find(|(n, _)| n == name) {
                    let rs = self.run_select(query, &[])?;
                    return Ok((rs.columns, rs.rows));
                }
                self.scan_table(name)
            }
            TableRef::Subquery { query, alias } => {
                let rs = self.run_select_scoped(query, &[], &[], ctes)?;
                let columns: Vec<String> = rs
                    .columns
                    .iter()
                    .map(|c| alloc::format!("{alias}.{c}"))
                    .collect();
                Ok((columns, rs.rows))
            }
        }
    }

    /// Scans all rows from a table, returning (columns, rows).
    fn scan_table(&self, table_name: &str) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        let ts = self
            .table(table_name)
            .ok_or_else(|| DbError::UnknownTable(table_name.to_string()))?;
        let txn = self
            .active_txns
            .iter()
            .find(|(n, _)| n == table_name)
            .map(|(_, t)| t);
        let mut rows = Vec::new();
        for id in 0..ts.next_row_id {
            if let Some(buffered) = txn.and_then(|t| t.get_write(id)) {
                if buffered[0] == MVCC_TOMBSTONE {
                    continue;
                }
                let row = decode_row_validated(id, &buffered[1..], ts.schema.columns.len())?;
                rows.push(row);
                continue;
            }
            if let Some(bytes) = ts.tree.get(id) {
                let row = decode_row_validated(id, bytes, ts.schema.columns.len())?;
                rows.push(row);
            }
        }
        Ok((ts.schema.columns.clone(), rows))
    }
}
