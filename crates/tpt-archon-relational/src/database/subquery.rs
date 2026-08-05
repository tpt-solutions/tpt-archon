//! Correlated-subquery detection and the uncorrelated-subquery result cache
//! used by `WHERE`/`HAVING` evaluation.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::executor::{self, Value};
use crate::parser::{CmpOp, Expr, SelectStatement, TableRef, CTE};

use super::schema::DbError;
use super::Database;

/// Pre-computed result for an uncorrelated subquery node, indexed by DFS
/// order. Correlated nodes get `Uncached` and are re-evaluated per row.
pub(super) enum CacheEntry {
    /// Subquery is correlated (references outer columns) — not cached.
    Uncached,
    /// `EXISTS(...)` result.
    Exists(bool),
    /// `column IN (SELECT ...)` — all values from the subquery's single column.
    In(Vec<Value>),
    /// `column <op> (SELECT ...)` — the single scalar value.
    Scalar(Value),
}

impl Database {
    /// Mirrors the local-only resolution logic of [`executor::find_value`]:
    /// does `name` resolve against `own_columns` without walking outer scopes?
    ///
    /// For qualified names (e.g. `"t.id"`), only exact matches are accepted —
    /// no suffix fallback, because that would be ambiguous across table
    /// qualifiers. For unqualified names, suffix matching is used (e.g. `"id"`
    /// matches `"t.id"`).
    fn column_resolves_locally(name: &str, own_columns: &[String]) -> bool {
        if own_columns.iter().any(|c| c == name) {
            return true;
        }
        if name.contains('.') {
            return false;
        }
        own_columns
            .iter()
            .any(|c| c.rfind('.').map_or(c.as_str(), |i| &c[i + 1..]) == name)
    }

    /// Returns `true` when `expr` references a column that isn't in
    /// `own_columns` (i.e. it references an outer scope — correlated).
    fn expr_references_outer(expr: &Expr, own_columns: &[String]) -> bool {
        match expr {
            Expr::Cmp { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::CmpColumn { left, right, .. } => {
                !Self::column_resolves_locally(left, own_columns)
                    || !Self::column_resolves_locally(right, own_columns)
            }
            Expr::IsNull { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::Like { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::InInt { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::BetweenInt { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::And(l, r) | Expr::Or(l, r) => {
                Self::expr_references_outer(l, own_columns)
                    || Self::expr_references_outer(r, own_columns)
            }
            Expr::Not(inner) => Self::expr_references_outer(inner, own_columns),
            Expr::Agg { .. } | Expr::AggCmp { .. } => false,
            Expr::ExtractCmp { source, .. } => !Self::column_resolves_locally(source, own_columns),
            Expr::Exists { query }
            | Expr::InSubquery { query, .. }
            | Expr::ScalarCmp { query, .. } => {
                if let Some(ref w) = query.filter {
                    let own = Self::resolve_query_own_columns(query, own_columns);
                    Self::expr_references_outer(w, &own)
                } else {
                    false
                }
            }
        }
    }

    /// Derives the column names visible from a subquery's own `FROM` + `JOIN`
    /// clauses, using the parent scope's column names as input. Columns are
    /// re-qualified with the subquery's own table aliases so that
    /// [`Self::column_resolves_locally`] can detect outer-scope references
    /// precisely.
    fn resolve_query_own_columns(
        query: &SelectStatement,
        parent_columns: &[String],
    ) -> Vec<String> {
        let mut cols = Vec::new();
        match &query.table {
            TableRef::Named { name, alias } => {
                let qualifier = alias.as_ref().unwrap_or(name);
                for c in parent_columns {
                    if let Some(dot) = c.find('.') {
                        let base_table = &c[..dot];
                        if base_table == name || alias.as_deref() == Some(base_table) {
                            cols.push(alloc::format!("{}.{}", qualifier, &c[dot + 1..]));
                        }
                    } else {
                        cols.push(alloc::format!("{}.{}", qualifier, c));
                    }
                }
            }
            TableRef::Subquery { .. } => cols.extend(parent_columns.iter().cloned()),
        }
        for join in &query.joins {
            let jt_name = join.table.table_name();
            let jt_qualifier = match &join.table {
                TableRef::Named { alias, .. } => {
                    alias.clone().unwrap_or_else(|| jt_name.to_string())
                }
                TableRef::Subquery { alias, .. } => alias.clone(),
            };
            for c in parent_columns {
                if let Some(dot) = c.find('.') {
                    let base_table = &c[..dot];
                    if base_table == jt_name || base_table == jt_qualifier.as_str() {
                        cols.push(alloc::format!("{}.{}", jt_qualifier, &c[dot + 1..]));
                    }
                }
            }
        }
        if cols.is_empty() {
            parent_columns.to_vec()
        } else {
            cols
        }
    }

    /// DFS-walks `expr`, assigning incrementing indices to each
    /// `Exists`/`InSubquery`/`ScalarCmp` node (matching the order used
    /// by [`Database::build_subquery_cache`] and [`Database::eval_where`]).
    fn walk_subqueries<F: FnMut(&mut usize, &Expr)>(
        expr: &mut Expr,
        counter: &mut usize,
        f: &mut F,
    ) {
        match expr {
            Expr::And(l, r) | Expr::Or(l, r) => {
                Self::walk_subqueries(l, counter, f);
                Self::walk_subqueries(r, counter, f);
            }
            Expr::Not(inner) => Self::walk_subqueries(inner, counter, f),
            Expr::Exists { .. } | Expr::InSubquery { .. } | Expr::ScalarCmp { .. } => {
                f(counter, expr);
            }
            _ => {}
        }
    }

    /// Pre-computes cache entries for uncorrelated subqueries in a `WHERE`
    /// expression tree. Each `Exists`/`InSubquery`/`ScalarCmp` node is
    /// visited in DFS order; if the subquery doesn't reference outer columns,
    /// its result is computed once and stored. Correlated nodes get
    /// `CacheEntry::Uncached`.
    pub(super) fn build_subquery_cache(
        &self,
        where_expr: &Expr,
        own_columns: &[String],
        params: &[Vec<f32>],
        outer_ctes: &[CTE],
    ) -> Result<Vec<CacheEntry>, DbError> {
        let mut cache = Vec::new();
        Self::walk_subqueries(&mut where_expr.clone(), &mut 0usize, &mut |_, _| {
            cache.push(CacheEntry::Uncached);
        });
        // Walk again with the same DFS order to populate.
        Self::walk_subqueries(
            &mut where_expr.clone(),
            &mut 0usize,
            &mut |counter, node| {
                let idx = *counter;
                *counter += 1;
                match node {
                    Expr::Exists { query }
                        if !Self::expr_references_outer(
                            &Expr::Exists {
                                query: query.clone(),
                            },
                            own_columns,
                        ) =>
                    {
                        if let Ok(rs) = self.run_select_scoped(query, params, &[], outer_ctes, None)
                        {
                            cache[idx] = CacheEntry::Exists(!rs.rows.is_empty());
                        }
                    }
                    Expr::InSubquery { column: _, query }
                        if !Self::expr_references_outer(
                            &Expr::InSubquery {
                                column: String::new(),
                                query: query.clone(),
                            },
                            own_columns,
                        ) =>
                    {
                        if let Ok(rs) = self.run_select_scoped(query, params, &[], outer_ctes, None)
                        {
                            let vals: Vec<Value> =
                                rs.rows.into_iter().map(|r| r[0].clone()).collect();
                            cache[idx] = CacheEntry::In(vals);
                        }
                    }
                    Expr::ScalarCmp {
                        column: _,
                        op: _,
                        query,
                    } if !Self::expr_references_outer(
                        &Expr::ScalarCmp {
                            column: String::new(),
                            op: CmpOp::Eq,
                            query: query.clone(),
                        },
                        own_columns,
                    ) =>
                    {
                        if let Ok(rs) = self.run_select_scoped(query, params, &[], outer_ctes, None)
                        {
                            if rs.rows.len() == 1 && rs.columns.len() == 1 {
                                cache[idx] = CacheEntry::Scalar(rs.rows[0][0].clone());
                            }
                        }
                    }
                    _ => {}
                }
            },
        );
        Ok(cache)
    }

    /// Evaluates a `WHERE`/`HAVING`-style predicate against a row, with
    /// database access for `Exists`/`InSubquery`/`ScalarCmp` nodes.
    ///
    /// Returns `Ok(Some(true))` / `Ok(Some(false))` / `Ok(None)` where `None`
    /// represents SQL `NULL` in a boolean context (Kleene three-valued logic).
    /// Callers that need a plain `bool` should call `.unwrap_or(false)` on the
    /// result — `NULL` is treated as false in `WHERE`/`HAVING` per SQL semantics.
    ///
    /// `And`/`Or`/`Not` recursion happens here (not in `executor::eval_expr`)
    /// so a subquery nested inside a boolean combinator still gets database
    /// access. `outer`, if given, is the enclosing query's `(columns, row)`
    /// — passed down so a correlated subquery's own `WHERE` can resolve a
    /// column that isn't in its own `FROM` (see `executor::find_value`).
    ///
    /// `outer_ctes` carries CTE definitions from enclosing queries so
    /// subqueries can reference them (subquery's own CTEs shadow outer ones).
    ///
    /// `cache` holds pre-computed results for uncorrelated subqueries (indexed
    /// by a DFS order over `Exists`/`InSubquery`/`ScalarCmp` nodes); `counter`
    /// advances through the cache as each node is visited.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_where(
        &self,
        expr: &Expr,
        columns: &[String],
        row: &[Value],
        params: &[Vec<f32>],
        outer: &[(&[String], &[Value])],
        outer_ctes: &[CTE],
        scope_columns: &[String],
        cache: &[CacheEntry],
        counter: &mut usize,
        valid_qualifiers: &[&str],
    ) -> Result<Option<bool>, DbError> {
        match expr {
            Expr::And(l, r) => {
                Ok(
                    match (
                        self.eval_where(
                            l,
                            columns,
                            row,
                            params,
                            outer,
                            outer_ctes,
                            scope_columns,
                            cache,
                            counter,
                            valid_qualifiers,
                        )?,
                        self.eval_where(
                            r,
                            columns,
                            row,
                            params,
                            outer,
                            outer_ctes,
                            scope_columns,
                            cache,
                            counter,
                            valid_qualifiers,
                        )?,
                    ) {
                        // Kleene AND: false AND _ = false; _ AND false = false
                        (Some(false), _) | (_, Some(false)) => Some(false),
                        // true AND true = true
                        (Some(true), Some(true)) => Some(true),
                        // NULL AND true = NULL; true AND NULL = NULL; NULL AND NULL = NULL
                        _ => None,
                    },
                )
            }
            Expr::Or(l, r) => {
                Ok(
                    match (
                        self.eval_where(
                            l,
                            columns,
                            row,
                            params,
                            outer,
                            outer_ctes,
                            scope_columns,
                            cache,
                            counter,
                            valid_qualifiers,
                        )?,
                        self.eval_where(
                            r,
                            columns,
                            row,
                            params,
                            outer,
                            outer_ctes,
                            scope_columns,
                            cache,
                            counter,
                            valid_qualifiers,
                        )?,
                    ) {
                        // Kleene OR: true OR _ = true; _ OR true = true
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        // false OR false = false
                        (Some(false), Some(false)) => Some(false),
                        // NULL OR false = NULL; false OR NULL = NULL; NULL OR NULL = NULL
                        _ => None,
                    },
                )
            }
            Expr::Not(inner) => {
                Ok(
                    match self.eval_where(
                        inner,
                        columns,
                        row,
                        params,
                        outer,
                        outer_ctes,
                        scope_columns,
                        cache,
                        counter,
                        valid_qualifiers,
                    )? {
                        Some(true) => Some(false),
                        Some(false) => Some(true),
                        None => None, // NOT NULL = NULL (Kleene)
                    },
                )
            }
            Expr::Exists { query } => {
                let idx = *counter;
                *counter += 1;
                if let Some(CacheEntry::Exists(result)) = cache.get(idx) {
                    return Ok(Some(*result));
                }
                let mut stack: [(&[String], &[Value]); 8] = [(&[], &[]); 8];
                let depth = 1 + outer.len();
                assert!(depth <= 8, "correlation nesting too deep");
                stack[0] = (scope_columns, row);
                stack[1..depth].copy_from_slice(outer);
                let rs =
                    self.run_select_scoped(query, params, &stack[..depth], outer_ctes, None)?;
                Ok(Some(!rs.rows.is_empty()))
            }
            Expr::InSubquery { column, query } => {
                let lhs = executor::find_value(column, columns, row, outer, valid_qualifiers)
                    .ok_or_else(|| DbError::UnknownColumn(column.clone()))?;
                let idx = *counter;
                *counter += 1;
                if let Some(CacheEntry::In(vals)) = cache.get(idx) {
                    return Ok(Some(vals.iter().any(|v| v == lhs)));
                }
                let mut stack: [(&[String], &[Value]); 8] = [(&[], &[]); 8];
                let depth = 1 + outer.len();
                assert!(depth <= 8, "correlation nesting too deep");
                stack[0] = (scope_columns, row);
                stack[1..depth].copy_from_slice(outer);
                let rs =
                    self.run_select_scoped(query, params, &stack[..depth], outer_ctes, None)?;
                if rs.columns.len() != 1 {
                    return Err(DbError::SubqueryCardinality(alloc::format!(
                        "IN subquery must return exactly one column, got {}",
                        rs.columns.len()
                    )));
                }
                Ok(Some(rs.rows.iter().any(|r| &r[0] == lhs)))
            }
            Expr::ScalarCmp { column, op, query } => {
                let lhs = executor::find_value(column, columns, row, outer, valid_qualifiers)
                    .ok_or_else(|| DbError::UnknownColumn(column.clone()))?;
                let idx = *counter;
                *counter += 1;
                if let Some(CacheEntry::Scalar(rhs)) = cache.get(idx) {
                    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
                        return Ok(Some(false));
                    }
                    let ord = lhs.cmp(rhs);
                    return Ok(Some(match op {
                        CmpOp::Eq => ord == core::cmp::Ordering::Equal,
                        CmpOp::Ne => ord != core::cmp::Ordering::Equal,
                        CmpOp::Lt => ord == core::cmp::Ordering::Less,
                        CmpOp::Le => ord != core::cmp::Ordering::Greater,
                        CmpOp::Gt => ord == core::cmp::Ordering::Greater,
                        CmpOp::Ge => ord != core::cmp::Ordering::Less,
                    }));
                }
                let mut stack: [(&[String], &[Value]); 8] = [(&[], &[]); 8];
                let depth = 1 + outer.len();
                assert!(depth <= 8, "correlation nesting too deep");
                stack[0] = (scope_columns, row);
                stack[1..depth].copy_from_slice(outer);
                let rs =
                    self.run_select_scoped(query, params, &stack[..depth], outer_ctes, None)?;
                if rs.columns.len() != 1 || rs.rows.len() != 1 {
                    return Err(DbError::SubqueryCardinality(alloc::format!(
                        "scalar subquery must return exactly one row and one column, got {} row(s) and {} column(s)",
                        rs.rows.len(),
                        rs.columns.len()
                    )));
                }
                let rhs = &rs.rows[0][0];
                if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
                    return Ok(Some(false));
                }
                let ord = lhs.cmp(rhs);
                Ok(Some(match op {
                    CmpOp::Eq => ord == core::cmp::Ordering::Equal,
                    CmpOp::Ne => ord != core::cmp::Ordering::Equal,
                    CmpOp::Lt => ord == core::cmp::Ordering::Less,
                    CmpOp::Le => ord != core::cmp::Ordering::Greater,
                    CmpOp::Gt => ord == core::cmp::Ordering::Greater,
                    CmpOp::Ge => ord != core::cmp::Ordering::Less,
                }))
            }
            _ => Ok(executor::eval_expr_scoped(expr, columns, row, outer, valid_qualifiers)?),
        }
    }
}
