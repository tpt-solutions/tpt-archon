//! `SELECT` execution, including vector top-k (`ORDER BY cosine(...)`).

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::executor::{self, Value};
use crate::parser::{
    CompoundStatement, JoinType, OrderByCosine, SelectStatement, SetOperation, TableRef, CTE,
};
use crate::planner::{plan_select, TableStats};
use crate::vector_index;

use super::codec::{decode_row_validated, MVCC_TOMBSTONE};
use super::schema::{ColumnType, DbError};
use super::{select_references_table, Database};

/// A name -> `(columns, rows)` override for a single table reference, used
/// only by [`Database::run_recursive_cte`] to bind a `WITH RECURSIVE` term's
/// self-reference to the previous iteration's working set (see
/// [`Database::run_select_scoped`]).
type RecursiveBinding<'a> = (&'a str, &'a [String], &'a [Vec<Value>]);

impl Database {
    /// Runs a top-level (non-correlated) `SELECT`.
    pub(super) fn run_select(
        &self,
        stmt: &SelectStatement,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        self.run_select_scoped(stmt, params, &[], &[], None)
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
    ///
    /// `recursive_binding`, when `Some((name, columns, rows))`, overrides
    /// name resolution for a single table reference: any `FROM`/`JOIN`
    /// naming `name` resolves directly to `(columns, rows)` instead of
    /// going through the normal CTE/view/table lookup. Used exclusively by
    /// [`Database::run_recursive_cte`] to bind a `WITH RECURSIVE` term's
    /// self-reference to the previous iteration's working set, without which
    /// evaluating the term would recurse into itself forever (the CTE
    /// definition is still in scope for its own body).
    pub(super) fn run_select_scoped(
        &self,
        stmt: &SelectStatement,
        params: &[Vec<f32>],
        outer: &[(&[String], &[Value])],
        outer_ctes: &[CTE],
        recursive_binding: Option<RecursiveBinding<'_>>,
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
            self.resolve_table_ref_with_ctes(&stmt.table, &merged_ctes, recursive_binding)?;

        // Process JOINs (nested-loop with general ON expression).
        for join in &stmt.joins {
            let (join_cols, join_rows) =
                self.resolve_table_ref_with_ctes(&join.table, &merged_ctes, recursive_binding)?;

            let mut new_cols = columns.clone();
            for rcol in &join_cols {
                new_cols.push(alloc::format!("{}.{}", join.table.name(), rcol));
            }

            let mut new_rows = Vec::new();

            match join.jtype {
                JoinType::Cross => {
                    for lrow in &rows {
                        for jrow in &join_rows {
                            let mut combined = lrow.clone();
                            combined.extend_from_slice(jrow);
                            new_rows.push(combined);
                        }
                    }
                }
                JoinType::Inner => {
                    let on = join.on_expr.as_ref().expect("INNER JOIN needs ON");
                    for lrow in &rows {
                        for jrow in &join_rows {
                            let mut combined = lrow.clone();
                            combined.extend_from_slice(jrow);
                            if executor::eval_expr(on, &new_cols, &combined)? {
                                new_rows.push(combined);
                            }
                        }
                    }
                }
                JoinType::Left => {
                    let on = join.on_expr.as_ref().expect("LEFT JOIN needs ON");
                    let null_right: Vec<Value> = core::iter::repeat(Value::Null)
                        .take(join_cols.len())
                        .collect();
                    for lrow in &rows {
                        let mut matched = false;
                        for jrow in &join_rows {
                            let mut combined = lrow.clone();
                            combined.extend_from_slice(jrow);
                            if executor::eval_expr(on, &new_cols, &combined)? {
                                new_rows.push(combined);
                                matched = true;
                            }
                        }
                        if !matched {
                            let mut combined = lrow.clone();
                            combined.extend_from_slice(&null_right);
                            new_rows.push(combined);
                        }
                    }
                }
                JoinType::Right => {
                    let on = join.on_expr.as_ref().expect("RIGHT JOIN needs ON");
                    let null_left: Vec<Value> = core::iter::repeat(Value::Null)
                        .take(columns.len())
                        .collect();
                    for jrow in &join_rows {
                        let mut matched = false;
                        for lrow in &rows {
                            let mut combined = lrow.clone();
                            combined.extend_from_slice(jrow);
                            if executor::eval_expr(on, &new_cols, &combined)? {
                                new_rows.push(combined);
                                matched = true;
                            }
                        }
                        if !matched {
                            let mut combined = null_left.clone();
                            combined.extend_from_slice(jrow);
                            new_rows.push(combined);
                        }
                    }
                }
                JoinType::Full => {
                    let on = join.on_expr.as_ref().expect("FULL JOIN needs ON");
                    let null_right: Vec<Value> = core::iter::repeat(Value::Null)
                        .take(join_cols.len())
                        .collect();
                    let null_left: Vec<Value> = core::iter::repeat(Value::Null)
                        .take(columns.len())
                        .collect();
                    let mut left_matched = vec![false; rows.len()];
                    for (li, lrow) in rows.iter().enumerate() {
                        for jrow in &join_rows {
                            let mut combined = lrow.clone();
                            combined.extend_from_slice(jrow);
                            if executor::eval_expr(on, &new_cols, &combined)? {
                                new_rows.push(combined);
                                left_matched[li] = true;
                            }
                        }
                    }
                    for (li, lrow) in rows.iter().enumerate() {
                        if !left_matched[li] {
                            let mut combined = lrow.clone();
                            combined.extend_from_slice(&null_right);
                            new_rows.push(combined);
                        }
                    }
                    for jrow in &join_rows {
                        let mut matched = false;
                        for lrow in &rows {
                            let mut combined = lrow.clone();
                            combined.extend_from_slice(jrow);
                            if executor::eval_expr(on, &new_cols, &combined)? {
                                matched = true;
                                break;
                            }
                        }
                        if !matched {
                            let mut combined = null_left.clone();
                            combined.extend_from_slice(jrow);
                            new_rows.push(combined);
                        }
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

        // Apply window functions (after WHERE, before GROUP BY/ORDER BY/
        // LIMIT — matching Postgres's logical query pipeline). Each call's
        // output becomes an ordinary extra column, so the existing
        // `Project` plan node picks it up by alias like any other column.
        if !stmt.window_funcs.is_empty() {
            executor::apply_window_funcs(&mut table, &stmt.window_funcs)?;
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
                ..Default::default()
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
            ..Default::default()
        })
    }

    /// Resolves a [`TableRef`] to `(columns, rows)` without CTE context.
    fn resolve_table_ref(&self, r: &TableRef) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        self.resolve_table_ref_with_ctes(r, &[], None)
    }

    /// Resolves a [`TableRef`] to `(columns, rows)`: checks `recursive_binding`
    /// first (see [`Database::run_select_scoped`]), then CTEs, then views,
    /// then real tables for `Named`; executes the inner query for `Subquery`.
    fn resolve_table_ref_with_ctes(
        &self,
        r: &TableRef,
        ctes: &[CTE],
        recursive_binding: Option<RecursiveBinding<'_>>,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        match r {
            TableRef::Named { name, .. } => {
                if let Some((bound_name, bound_cols, bound_rows)) = recursive_binding {
                    if bound_name == name {
                        return Ok((bound_cols.to_vec(), bound_rows.to_vec()));
                    }
                }
                if let Some(cte) = ctes.iter().find(|c| &c.name == name) {
                    if cte.recursive_term.is_some() {
                        return self.run_recursive_cte(cte, ctes);
                    }
                    let rs = self.run_select_scoped(&cte.query, &[], &[], ctes, None)?;
                    return Ok((rs.columns, rs.rows));
                }
                if let Some((_, query)) = self.views.iter().find(|(n, _)| n == name) {
                    let rs = self.run_select(query, &[])?;
                    return Ok((rs.columns, rs.rows));
                }
                self.scan_table(name)
            }
            TableRef::Subquery { query, alias } => {
                let rs = self.run_select_scoped(query, &[], &[], ctes, None)?;
                let columns: Vec<String> = rs
                    .columns
                    .iter()
                    .map(|c| alloc::format!("{alias}.{c}"))
                    .collect();
                Ok((columns, rs.rows))
            }
        }
    }

    /// Evaluates a `WITH RECURSIVE` CTE to a fixed point: runs the anchor
    /// term once, then repeatedly runs the recursive term with the CTE's
    /// name bound (via `recursive_binding`) to only the *previous*
    /// iteration's new rows (the standard "working table" semantics — each
    /// row is fed through the recursive term exactly once, not once per
    /// total-so-far row), accumulating results until an iteration produces
    /// no new rows. A hard iteration cap guards against a recursive term
    /// that never shrinks to an empty working set (e.g. a query with no
    /// base-case filter), turning what would otherwise be an unbounded loop
    /// into a normal `DbError`.
    fn run_recursive_cte(
        &self,
        cte: &CTE,
        ctes: &[CTE],
    ) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        const MAX_RECURSIVE_ITERATIONS: usize = 10_000;

        let (set_op, recursive_query) = cte
            .recursive_term
            .as_ref()
            .expect("caller checked recursive_term.is_some()");
        let dedup = matches!(set_op, SetOperation::Union(false));

        let anchor_rs = self.run_select_scoped(&cte.query, &[], &[], ctes, None)?;
        let columns = anchor_rs.columns;
        let mut all_rows = anchor_rs.rows.clone();
        let mut working = anchor_rs.rows;
        if dedup {
            all_rows.sort();
            all_rows.dedup();
            working = all_rows.clone();
        }

        let mut iterations = 0usize;
        while !working.is_empty() {
            iterations += 1;
            if iterations > MAX_RECURSIVE_ITERATIONS {
                return Err(DbError::Unsupported(alloc::format!(
                    "recursive CTE '{}' exceeded {} iterations without reaching a fixed \
                     point (likely a non-terminating recursive term)",
                    cte.name,
                    MAX_RECURSIVE_ITERATIONS
                )));
            }
            let binding = (cte.name.as_str(), columns.as_slice(), working.as_slice());
            let step_rs = self.run_select_scoped(recursive_query, &[], &[], ctes, Some(binding))?;
            if step_rs.columns.len() != columns.len() {
                return Err(DbError::ColumnCountMismatch);
            }
            let mut next_working = step_rs.rows;
            if dedup {
                next_working.retain(|r| !all_rows.contains(r));
                next_working.sort();
                next_working.dedup();
            }
            if next_working.is_empty() {
                break;
            }
            all_rows.extend(next_working.iter().cloned());
            working = next_working;
        }

        Ok((columns, all_rows))
    }

    /// Executes a compound statement: `SELECT ... UNION/INTERSECT/EXCEPT SELECT ...`
    /// with optional final ORDER BY / LIMIT.
    pub(super) fn run_compound(
        &self,
        stmt: &CompoundStatement,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        // Run the first SELECT.
        let (columns, mut rows) = {
            let first = self.run_select_scoped(&stmt.first, params, &[], &[], None)?;
            (first.columns, first.rows)
        };

        for (op, right) in &stmt.operations {
            let right_rs = self.run_select_scoped(right, params, &[], &[], None)?;

            // Column count must match.
            if columns.len() != right_rs.columns.len() {
                return Err(DbError::ColumnCountMismatch);
            }

            match op {
                SetOperation::Union(all) => {
                    rows.extend(right_rs.rows);
                    if !*all {
                        rows.sort();
                        rows.dedup();
                    }
                }
                SetOperation::Intersect => {
                    let mut result = Vec::new();
                    for row in &rows {
                        if right_rs.rows.contains(row) {
                            result.push(row.clone());
                        }
                    }
                    rows = result;
                    rows.sort();
                    rows.dedup();
                }
                SetOperation::Except => {
                    rows.retain(|row| !right_rs.rows.contains(row));
                }
            }
        }

        // Apply compound-level ORDER BY (if any).
        if !stmt.order_by.is_empty() {
            let sort_indices: Vec<(usize, bool)> = stmt
                .order_by
                .iter()
                .map(|ob| {
                    let idx = columns
                        .iter()
                        .position(|c| c == &ob.column)
                        .ok_or_else(|| DbError::UnknownColumn(ob.column.clone()))?;
                    Ok((idx, ob.descending))
                })
                .collect::<Result<Vec<(usize, bool)>, DbError>>()?;
            rows.sort_by(|a, b| {
                for &(idx, desc) in &sort_indices {
                    let ord = a[idx].cmp(&b[idx]);
                    let ord = if desc { ord.reverse() } else { ord };
                    if ord != core::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                core::cmp::Ordering::Equal
            });
        }

        // Apply compound-level LIMIT (if any).
        if let Some(limit) = stmt.limit {
            rows.truncate(limit as usize);
        }

        Ok(executor::ResultSet {
            columns,
            rows,
            ..Default::default()
        })
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

#[cfg(test)]
mod tests {
    use crate::database::Database;
    use crate::parser::parse_statement;

    #[test]
    fn test_union_all() {
        let mut db = Database::empty();
        for sql in [
            "CREATE TABLE t1 (x INT)",
            "INSERT INTO t1 (x) VALUES (1)",
            "INSERT INTO t1 (x) VALUES (2)",
        ] {
            let stmt = parse_statement(sql).unwrap();
            db.execute(&stmt, &[]).unwrap();
        }
        let sql = "SELECT x FROM t1 UNION ALL SELECT x FROM t1";
        let stmt = parse_statement(sql).unwrap();
        let rs = db.execute(&stmt, &[]).unwrap();
        // UNION ALL of two identical tables: 1,2,1,2 (4 rows)
        assert_eq!(rs.columns, vec!["x"]);
        assert_eq!(rs.rows.len(), 4);
    }

    #[test]
    fn test_union_dedup() {
        let mut db = Database::empty();
        for sql in [
            "CREATE TABLE t1 (x INT)",
            "INSERT INTO t1 (x) VALUES (1)",
            "INSERT INTO t1 (x) VALUES (2)",
        ] {
            let stmt = parse_statement(sql).unwrap();
            db.execute(&stmt, &[]).unwrap();
        }
        let sql = "SELECT x FROM t1 UNION SELECT x FROM t1";
        let stmt = parse_statement(sql).unwrap();
        let rs = db.execute(&stmt, &[]).unwrap();
        // UNION of identical tables: dedup to 1,2 (sorted) → 2 rows
        assert_eq!(rs.columns, vec!["x"]);
        assert_eq!(rs.rows.len(), 2);
    }

    #[test]
    fn test_intersect() {
        let mut db = Database::empty();
        for sql in [
            "CREATE TABLE t1 (x INT)",
            "CREATE TABLE t2 (x INT)",
            "INSERT INTO t1 (x) VALUES (1)",
            "INSERT INTO t1 (x) VALUES (2)",
            "INSERT INTO t1 (x) VALUES (3)",
            "INSERT INTO t2 (x) VALUES (2)",
            "INSERT INTO t2 (x) VALUES (3)",
            "INSERT INTO t2 (x) VALUES (4)",
        ] {
            let stmt = parse_statement(sql).unwrap();
            db.execute(&stmt, &[]).unwrap();
        }
        let sql = "SELECT x FROM t1 INTERSECT SELECT x FROM t2";
        let stmt = parse_statement(sql).unwrap();
        let rs = db.execute(&stmt, &[]).unwrap();
        // Intersect: {1,2,3} ∩ {2,3,4} = {2,3} (sorted) → 2 rows
        assert_eq!(rs.columns, vec!["x"]);
        assert_eq!(rs.rows.len(), 2);
    }

    #[test]
    fn test_except() {
        let mut db = Database::empty();
        for sql in [
            "CREATE TABLE t1 (x INT)",
            "CREATE TABLE t2 (x INT)",
            "INSERT INTO t1 (x) VALUES (1)",
            "INSERT INTO t1 (x) VALUES (2)",
            "INSERT INTO t1 (x) VALUES (3)",
            "INSERT INTO t2 (x) VALUES (2)",
            "INSERT INTO t2 (x) VALUES (3)",
            "INSERT INTO t2 (x) VALUES (4)",
        ] {
            let stmt = parse_statement(sql).unwrap();
            db.execute(&stmt, &[]).unwrap();
        }
        let sql = "SELECT x FROM t1 EXCEPT SELECT x FROM t2";
        let stmt = parse_statement(sql).unwrap();
        let rs = db.execute(&stmt, &[]).unwrap();
        // Except: {1,2,3} \ {2,3,4} = {1} → 1 row
        assert_eq!(rs.columns, vec!["x"]);
        assert_eq!(rs.rows.len(), 1);
    }

    #[test]
    fn test_union_column_mismatch_errors() {
        let mut db = Database::empty();
        for sql in ["CREATE TABLE t1 (x INT)", "CREATE TABLE t2 (y INT, z INT)"] {
            let stmt = parse_statement(sql).unwrap();
            db.execute(&stmt, &[]).unwrap();
        }
        let sql = "SELECT x FROM t1 UNION SELECT y, z FROM t2";
        let stmt = parse_statement(sql).unwrap();
        let result = db.execute(&stmt, &[]);
        assert!(result.is_err(), "expected ColumnCountMismatch but got Ok");
    }

    #[test]
    fn test_join_unknown_column() {
        let mut db = Database::empty();
        for sql in [
            "CREATE TABLE t3 (v INT)",
            "CREATE TABLE t4 (t3_ref INT)",
            "INSERT INTO t3 (v) VALUES (1)",
            "INSERT INTO t4 (t3_ref) VALUES (1)",
        ] {
            let stmt = parse_statement(sql).unwrap();
            db.execute(&stmt, &[]).unwrap();
        }
        let sql = "SELECT v FROM t3 JOIN t4 ON t3.no_such_col = t4.t3_ref";
        let stmt = parse_statement(sql).unwrap();
        let result = db.execute(&stmt, &[]);
        assert!(result.is_err(), "expected error but got Ok: {result:?}");
        assert!(
            format!("{:?}", result.as_ref().unwrap_err()).contains("UnknownColumn"),
            "wrong error: {result:?}"
        );
    }

    fn employees_db() -> Database {
        let mut db = Database::empty();
        for sql in [
            "CREATE TABLE employees (dept TEXT, salary INT)",
            "INSERT INTO employees (dept, salary) VALUES ('eng', 100)",
            "INSERT INTO employees (dept, salary) VALUES ('eng', 200)",
            "INSERT INTO employees (dept, salary) VALUES ('eng', 200)",
            "INSERT INTO employees (dept, salary) VALUES ('sales', 50)",
            "INSERT INTO employees (dept, salary) VALUES ('sales', 150)",
        ] {
            db.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
        }
        db
    }

    fn int_col(rs: &crate::executor::ResultSet, col: &str) -> Vec<i64> {
        let idx = rs.columns.iter().position(|c| c == col).unwrap();
        rs.rows
            .iter()
            .map(|r| match &r[idx] {
                crate::executor::Value::Int(n) => *n,
                other => panic!("expected Int in column {col}, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn window_row_number_per_partition() {
        let mut db = employees_db();
        let sql = "SELECT dept, salary, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) AS rn \
                    FROM employees ORDER BY dept, rn";
        let rs = db.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
        assert_eq!(int_col(&rs, "rn"), vec![1, 2, 3, 1, 2]);
        assert_eq!(int_col(&rs, "salary"), vec![200, 200, 100, 150, 50]);
    }

    #[test]
    fn window_rank_and_dense_rank_share_ties() {
        let mut db = employees_db();
        let sql = "SELECT salary, RANK() OVER (PARTITION BY dept ORDER BY salary DESC) AS r, \
                    DENSE_RANK() OVER (PARTITION BY dept ORDER BY salary DESC) AS dr \
                    FROM employees WHERE dept = 'eng' ORDER BY r";
        let rs = db.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
        // eng salaries sorted desc: 200, 200, 100 -> RANK: 1,1,3; DENSE_RANK: 1,1,2
        assert_eq!(int_col(&rs, "r"), vec![1, 1, 3]);
        assert_eq!(int_col(&rs, "dr"), vec![1, 1, 2]);
    }

    #[test]
    fn window_lag_lead_with_default() {
        let mut db = employees_db();
        let sql = "SELECT salary, \
                    LAG(salary, 1, 0) OVER (PARTITION BY dept ORDER BY salary) AS prev, \
                    LEAD(salary, 1, 0) OVER (PARTITION BY dept ORDER BY salary) AS next \
                    FROM employees WHERE dept = 'sales' ORDER BY salary";
        let rs = db.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
        // sales sorted asc: 50, 150
        assert_eq!(int_col(&rs, "salary"), vec![50, 150]);
        assert_eq!(int_col(&rs, "prev"), vec![0, 50]);
        assert_eq!(int_col(&rs, "next"), vec![150, 0]);
    }

    #[test]
    fn window_sum_running_total_default_frame() {
        let mut db = employees_db();
        // No explicit frame + an ORDER BY -> default UNBOUNDED PRECEDING ..
        // CURRENT ROW (a running total).
        let sql = "SELECT salary, SUM(salary) OVER (PARTITION BY dept ORDER BY salary) AS running \
                    FROM employees WHERE dept = 'eng' ORDER BY salary";
        let rs = db.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
        assert_eq!(int_col(&rs, "salary"), vec![100, 200, 200]);
        assert_eq!(int_col(&rs, "running"), vec![100, 300, 500]);
    }

    #[test]
    fn window_sum_whole_partition_when_no_order_by() {
        let mut db = employees_db();
        // No ORDER BY in OVER(...) -> default frame is the whole partition.
        let sql = "SELECT salary, SUM(salary) OVER (PARTITION BY dept) AS total \
                    FROM employees WHERE dept = 'eng' ORDER BY salary";
        let rs = db.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
        assert_eq!(int_col(&rs, "total"), vec![500, 500, 500]);
    }

    #[test]
    fn window_explicit_rows_between() {
        let mut db = employees_db();
        // 2-row trailing window: current + 1 preceding.
        let sql = "SELECT salary, SUM(salary) OVER ( \
                        PARTITION BY dept ORDER BY salary \
                        ROWS BETWEEN 1 PRECEDING AND CURRENT ROW \
                    ) AS win_sum \
                    FROM employees WHERE dept = 'eng' ORDER BY salary";
        let rs = db.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
        // eng sorted asc: 100, 200, 200 -> windows: [100]=100, [100,200]=300, [200,200]=400
        assert_eq!(int_col(&rs, "salary"), vec![100, 200, 200]);
        assert_eq!(int_col(&rs, "win_sum"), vec![100, 300, 400]);
    }

    #[test]
    fn window_range_frame_is_rejected() {
        let sql = "SELECT salary, SUM(salary) OVER (ORDER BY salary RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) \
                    FROM employees";
        assert!(parse_statement(sql).is_err());
    }

    #[test]
    fn window_count_star_over() {
        let mut db = employees_db();
        let sql =
            "SELECT dept, COUNT(*) OVER (PARTITION BY dept) AS c FROM employees ORDER BY dept";
        let rs = db.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
        assert_eq!(int_col(&rs, "c"), vec![3, 3, 3, 2, 2]);
    }
}
