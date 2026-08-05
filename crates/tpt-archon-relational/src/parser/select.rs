//! `SELECT` parsing: columns/aggregates, `FROM`/`JOIN`, `WHERE`/`GROUP BY`/
//! `HAVING`/`ORDER BY`/`LIMIT`, plus the `WITH` clause and table-reference
//! parsing (including derived-table subqueries) that assemble a `SELECT`'s
//! sources.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::ast::{
    AggregateFunc, CompoundStatement, Expr, FrameBound, Join, JoinType, OrderBy, OrderByCosine,
    ParseError, SelectLiteralItem, SelectStatement, SetOperation, Statement, TableRef, WindowCall,
    WindowFrame, WindowFunc, WindowSpec, CTE,
};
use super::expr::{parse_expr, parse_literal};
use super::lexer::{
    eq_ignore_case, expect_ident, expect_int, expect_kw, expect_tok, Tok, TokenStream,
};

// ---------------------------------------------------------------------------
// SELECT
// ---------------------------------------------------------------------------

/// Parses a complete SELECT (core + optional tail). Used for simple SELECT
/// statements and for subqueries.
fn parse_select_full(ts: &mut TokenStream) -> Result<SelectStatement, ParseError> {
    let mut stmt = parse_select_core(ts)?;
    let mut obc = stmt.order_by_cosine;
    parse_select_tail(ts, &mut stmt.order_by, &mut stmt.limit, &mut obc)?;
    stmt.order_by_cosine = obc;
    Ok(stmt)
}

pub(super) fn parse_select_inner(ts: &mut TokenStream) -> Result<SelectStatement, ParseError> {
    ts.enter_depth()?;
    let result = parse_select_full(ts);
    ts.exit_depth();
    result
}

/// Attempts to parse a FROM-less `SELECT <literal> [AS alias], ...` (e.g.
/// `SELECT 1`, `SELECT 'x' AS greeting`) — the common driver health-check
/// query shape (spec fact #3: `FROM` was unconditionally mandatory, and
/// there was no scalar-literal SELECT list at all to make `SELECT 1` work
/// even if it were optional). Returns `None` and restores `ts`'s position
/// if the input isn't this shape (e.g. it's a real `SELECT col FROM t`, or
/// a literal list immediately followed by `FROM`), so the caller falls
/// through to the normal column/FROM-based parser — this never partially
/// commits.
pub(super) fn try_parse_select_literal(ts: &mut TokenStream) -> Option<Vec<SelectLiteralItem>> {
    let sp = ts.save();
    let mut items = Vec::new();
    loop {
        let value = match parse_literal(ts) {
            Ok(v) => v,
            Err(_) => {
                ts.restore(sp);
                return None;
            }
        };
        let alias = match parse_optional_alias(ts) {
            Ok(a) => a,
            Err(_) => {
                ts.restore(sp);
                return None;
            }
        };
        items.push(SelectLiteralItem { value, alias });
        if let Tok::Comma = ts.peek() {
            ts.next();
            continue;
        }
        break;
    }
    if !matches!(ts.peek(), Tok::Eof) {
        ts.restore(sp);
        return None;
    }
    Some(items)
}

/// Parses either a simple SELECT or a compound query (UNION/INTERSECT/EXCEPT).
/// Used at the top level of `parse_statement`.
pub(super) fn parse_select_or_compound(
    ts: &mut TokenStream,
    ctes: Vec<CTE>,
) -> Result<Statement, ParseError> {
    let first = parse_select_core(ts)?;

    // Check for set operations.
    let mut operations = Vec::new();
    loop {
        let sp = ts.save();
        let op = match ts.peek() {
            Tok::Ident(kw) => {
                if eq_ignore_case(&kw, "union") {
                    ts.next();
                    let all = if let Tok::Ident(kw2) = ts.peek() {
                        if eq_ignore_case(&kw2, "all") {
                            ts.next();
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    SetOperation::Union(all)
                } else if eq_ignore_case(&kw, "intersect") {
                    ts.next();
                    SetOperation::Intersect
                } else if eq_ignore_case(&kw, "except") {
                    ts.next();
                    SetOperation::Except
                } else {
                    ts.restore(sp);
                    break;
                }
            }
            _ => {
                ts.restore(sp);
                break;
            }
        };
        // Consume the SELECT keyword before the right-hand SELECT core.
        if !matches!(ts.next(), Tok::Ident(kw) if eq_ignore_case(&kw, "select")) {
            return Err(ParseError(
                "expected SELECT after set operation".to_string(),
            ));
        }
        let right = parse_select_core(ts)?;
        operations.push((op, right));
    }

    if operations.is_empty() {
        // Simple SELECT: apply CTEs and parse tail.
        let mut stmt = first;
        stmt.with_ctes = ctes;
        parse_select_tail(
            ts,
            &mut stmt.order_by,
            &mut stmt.limit,
            &mut stmt.order_by_cosine,
        )?;
        Ok(Statement::Select(stmt))
    } else {
        // Compound: parse tail for the compound (ORDER BY / LIMIT apply to
        // the entire combined result, not to individual operands).
        let mut order_by = Vec::new();
        let mut limit = None;
        parse_select_tail(ts, &mut order_by, &mut limit, &mut None)?;
        Ok(Statement::Compound(CompoundStatement {
            first: Box::new(first),
            operations,
            order_by,
            limit,
        }))
    }
}

/// Parses a "select core": column list, FROM, JOINs, WHERE, GROUP BY, HAVING.
/// Does NOT parse ORDER BY, LIMIT, or `order_by_cosine` (those belong either to
/// the compound tail or to a simple SELECT via `parse_select_tail`).
fn parse_select_core(ts: &mut TokenStream) -> Result<SelectStatement, ParseError> {
    let mut columns = Vec::new();
    let mut star = false;
    let mut aggregates = Vec::new();
    let mut window_funcs = Vec::new();

    if let Tok::Star = ts.peek() {
        star = true;
        ts.next();
    } else {
        loop {
            match ts.next() {
                Tok::Ident(name) => {
                    let col_name = name.clone();
                    let lower = col_name.to_ascii_lowercase();
                    let agg_func = match lower.as_str() {
                        "count" => Some(AggregateFunc::Count),
                        "sum" => Some(AggregateFunc::Sum),
                        "avg" => Some(AggregateFunc::Avg),
                        "min" => Some(AggregateFunc::Min),
                        "max" => Some(AggregateFunc::Max),
                        _ => None,
                    };
                    let window_only = matches!(
                        lower.as_str(),
                        "row_number" | "rank" | "dense_rank" | "lag" | "lead"
                    );
                    // Only treat this identifier as an aggregate/window call if
                    // a '(' actually follows — otherwise it's a plain column
                    // that happens to share a name with a function (e.g. a
                    // `count` column), which must still parse.
                    if window_only && matches!(ts.peek(), Tok::LParen) {
                        ts.next();
                        let func = parse_window_only_func(ts, &lower)?;
                        expect_tok(ts, Tok::RParen, "')'")?;
                        expect_kw(ts, "over")?;
                        let spec = parse_window_spec(ts)?;
                        let alias = parse_optional_alias(ts)?
                            .unwrap_or_else(|| window_default_alias(&lower));
                        window_funcs.push((alias.clone(), WindowCall { func, spec }));
                        columns.push(alias);
                    } else if let Some(agg) = agg_func.filter(|_| matches!(ts.peek(), Tok::LParen))
                    {
                        ts.next();
                        let inner = if let Tok::Star = ts.peek() {
                            ts.next();
                            expect_tok(ts, Tok::RParen, "')'")?;
                            "*".to_string()
                        } else {
                            let c = expect_ident(ts, "column in aggregate")?;
                            expect_tok(ts, Tok::RParen, "')'")?;
                            c
                        };
                        // An aggregate immediately followed by OVER is a
                        // window function, not a GROUP BY aggregate.
                        let is_window = matches!(
                            ts.peek(),
                            Tok::Ident(ref kw) if eq_ignore_case(kw, "over")
                        );
                        if is_window {
                            ts.next();
                            let spec = parse_window_spec(ts)?;
                            let alias = parse_optional_alias(ts)?
                                .unwrap_or_else(|| crate::executor::agg_default_alias(agg, &inner));
                            window_funcs.push((
                                alias.clone(),
                                WindowCall {
                                    func: WindowFunc::Agg {
                                        func: agg,
                                        column: inner,
                                    },
                                    spec,
                                },
                            ));
                            columns.push(alias);
                        } else {
                            let alias = parse_optional_alias(ts)?
                                .unwrap_or_else(|| crate::executor::agg_default_alias(agg, &inner));
                            aggregates.push((alias.clone(), agg, inner));
                            columns.push(alias);
                        }
                    } else {
                        let mut col_name = col_name;
                        if let Tok::Dot = ts.peek() {
                            ts.next();
                            let field = expect_ident(ts, "field name after '.'")?;
                            col_name = alloc::format!("{col_name}.{field}");
                        }
                        if let Tok::Ident(kw) = ts.peek() {
                            if eq_ignore_case(&kw, "as") {
                                ts.next();
                                let alias = expect_ident(ts, "alias")?;
                                columns.push(alias);
                            } else {
                                columns.push(col_name);
                            }
                        } else {
                            columns.push(col_name);
                        }
                    }
                }
                _ => return Err(ParseError("expected column name".to_string())),
            }
            if let Tok::Comma = ts.peek() {
                ts.next();
                continue;
            }
            break;
        }
    }

    match ts.next() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "from") => {}
        _ => return Err(ParseError("expected FROM".to_string())),
    }
    let table = parse_table_ref(ts)?;

    let mut joins = Vec::new();
    loop {
        let sp = ts.save();
        let (jtype, has_on) = match ts.peek() {
            Tok::Ident(kw) if eq_ignore_case(&kw, "left") => {
                ts.next();
                if let Tok::Ident(kw2) = ts.peek() {
                    if eq_ignore_case(&kw2, "outer") {
                        ts.next();
                    }
                }
                expect_kw(ts, "join")?;
                (JoinType::Left, true)
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "right") => {
                ts.next();
                if let Tok::Ident(kw2) = ts.peek() {
                    if eq_ignore_case(&kw2, "outer") {
                        ts.next();
                    }
                }
                expect_kw(ts, "join")?;
                (JoinType::Right, true)
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "full") => {
                ts.next();
                if let Tok::Ident(kw2) = ts.peek() {
                    if eq_ignore_case(&kw2, "outer") {
                        ts.next();
                    }
                }
                expect_kw(ts, "join")?;
                (JoinType::Full, true)
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "cross") => {
                ts.next();
                expect_kw(ts, "join")?;
                (JoinType::Cross, false)
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "join") || eq_ignore_case(&kw, "inner") => {
                if eq_ignore_case(&kw, "inner") {
                    ts.next();
                }
                expect_kw(ts, "join")?;
                (JoinType::Inner, true)
            }
            _ => {
                ts.restore(sp);
                break;
            }
        };
        let join_table = parse_table_ref(ts)?;
        let on_expr = if has_on {
            expect_kw(ts, "on")?;
            Some(parse_expr(ts)?)
        } else {
            None
        };
        joins.push(Join {
            jtype,
            table: join_table,
            on_expr,
        });
    }

    let mut filter = None;
    let mut group_by = Vec::new();
    let mut having = None;

    loop {
        match ts.peek() {
            Tok::Ident(kw) if eq_ignore_case(&kw, "where") => {
                ts.next();
                if filter.is_some() {
                    return Err(ParseError("duplicate WHERE".to_string()));
                }
                filter = Some(parse_expr(ts)?);
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "group") => {
                ts.next();
                expect_kw(ts, "by")?;
                loop {
                    let col = expect_ident(ts, "column in GROUP BY")?;
                    group_by.push(col);
                    if let Tok::Comma = ts.peek() {
                        ts.next();
                        continue;
                    }
                    break;
                }
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "having") => {
                ts.next();
                if having.is_some() {
                    return Err(ParseError("duplicate HAVING".to_string()));
                }
                having = Some(parse_expr(ts)?);
            }
            _ => break,
        }
    }

    if let Some(hv) = having.as_mut() {
        resolve_having_aliases(hv, &aggregates);
    }

    Ok(SelectStatement {
        columns,
        star,
        table,
        filter,
        joins,
        group_by,
        aggregates,
        window_funcs,
        order_by: Vec::new(),
        order_by_cosine: None,
        limit: None,
        having,
        with_ctes: Vec::new(),
    })
}

/// Parses `func()`'s zero/one/two/three-argument list for a window-only
/// function (`ROW_NUMBER`/`RANK`/`DENSE_RANK` take none; `LAG`/`LEAD` take
/// `(column [, offset [, default]])`), given the already-lowercased function
/// name and with the opening `(` already consumed.
fn parse_window_only_func(ts: &mut TokenStream, lower: &str) -> Result<WindowFunc, ParseError> {
    match lower {
        "row_number" => Ok(WindowFunc::RowNumber),
        "rank" => Ok(WindowFunc::Rank),
        "dense_rank" => Ok(WindowFunc::DenseRank),
        "lag" | "lead" => {
            let column = expect_ident(ts, "column in LAG/LEAD")?;
            let mut offset: i64 = 1;
            let mut default = None;
            if let Tok::Comma = ts.peek() {
                ts.next();
                offset = expect_int(ts, "offset in LAG/LEAD")?;
                if let Tok::Comma = ts.peek() {
                    ts.next();
                    default = Some(parse_literal(ts)?);
                }
            }
            if lower == "lag" {
                Ok(WindowFunc::Lag {
                    column,
                    offset,
                    default,
                })
            } else {
                Ok(WindowFunc::Lead {
                    column,
                    offset,
                    default,
                })
            }
        }
        _ => unreachable!("caller only dispatches known window-only function names"),
    }
}

/// Default column alias for a window-only function call with no explicit
/// `AS alias` (`ROW_NUMBER()` -> `"row_number"`, etc; matches
/// `executor::agg_default_alias`'s style for plain aggregates).
fn window_default_alias(lower: &str) -> String {
    lower.to_string()
}

/// Parses `OVER (PARTITION BY ... ORDER BY ... [frame])` with the `OVER`
/// keyword already consumed.
fn parse_window_spec(ts: &mut TokenStream) -> Result<WindowSpec, ParseError> {
    expect_tok(ts, Tok::LParen, "'(' after OVER")?;

    let mut partition_by = Vec::new();
    if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "partition") {
            ts.next();
            expect_kw(ts, "by")?;
            loop {
                partition_by.push(expect_ident(ts, "column in PARTITION BY")?);
                if let Tok::Comma = ts.peek() {
                    ts.next();
                    continue;
                }
                break;
            }
        }
    }

    let mut order_by = Vec::new();
    if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "order") {
            ts.next();
            expect_kw(ts, "by")?;
            loop {
                let column = expect_ident(ts, "column in window ORDER BY")?;
                let mut descending = false;
                if let Tok::Ident(kw2) = ts.peek() {
                    if eq_ignore_case(&kw2, "desc") {
                        descending = true;
                        ts.next();
                    } else if eq_ignore_case(&kw2, "asc") {
                        ts.next();
                    }
                }
                order_by.push(OrderBy { column, descending });
                if let Tok::Comma = ts.peek() {
                    ts.next();
                    continue;
                }
                break;
            }
        }
    }

    let frame = if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "rows") {
            ts.next();
            Some(parse_window_frame(ts)?)
        } else if eq_ignore_case(&kw, "range") || eq_ignore_case(&kw, "groups") {
            return Err(ParseError(
                "RANGE/GROUPS window frames are not supported (ROWS with numeric offsets \
                 only)"
                    .to_string(),
            ));
        } else {
            None
        }
    } else {
        None
    };

    expect_tok(ts, Tok::RParen, "')' to close OVER (...)")?;
    Ok(WindowSpec {
        partition_by,
        order_by,
        frame,
    })
}

/// Parses a `ROWS` frame body (the `ROWS` keyword already consumed): either
/// `BETWEEN <bound> AND <bound>` or a single `<bound>` (meaning
/// `BETWEEN <bound> AND CURRENT ROW`, matching Postgres's shorthand form).
fn parse_window_frame(ts: &mut TokenStream) -> Result<WindowFrame, ParseError> {
    if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "between") {
            ts.next();
            let start = parse_frame_bound(ts)?;
            expect_kw(ts, "and")?;
            let end = parse_frame_bound(ts)?;
            return Ok(WindowFrame { start, end });
        }
    }
    let start = parse_frame_bound(ts)?;
    Ok(WindowFrame {
        start,
        end: FrameBound::CurrentRow,
    })
}

fn parse_frame_bound(ts: &mut TokenStream) -> Result<FrameBound, ParseError> {
    match ts.peek() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "unbounded") => {
            ts.next();
            match ts.next() {
                Tok::Ident(k) if eq_ignore_case(&k, "preceding") => {
                    Ok(FrameBound::UnboundedPreceding)
                }
                Tok::Ident(k) if eq_ignore_case(&k, "following") => {
                    Ok(FrameBound::UnboundedFollowing)
                }
                _ => Err(ParseError(
                    "expected PRECEDING or FOLLOWING after UNBOUNDED".to_string(),
                )),
            }
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "current") => {
            ts.next();
            expect_kw(ts, "row")?;
            Ok(FrameBound::CurrentRow)
        }
        Tok::Int(n) if n >= 0 => {
            ts.next();
            match ts.next() {
                Tok::Ident(k) if eq_ignore_case(&k, "preceding") => {
                    Ok(FrameBound::Preceding(n as u64))
                }
                Tok::Ident(k) if eq_ignore_case(&k, "following") => {
                    Ok(FrameBound::Following(n as u64))
                }
                _ => Err(ParseError(
                    "expected PRECEDING or FOLLOWING after a numeric frame offset".to_string(),
                )),
            }
        }
        _ => Err(ParseError(
            "expected UNBOUNDED, CURRENT ROW, or a non-negative integer frame bound".to_string(),
        )),
    }
}

/// Parses an optional `AS alias` (the only alias form this grammar accepts
/// for aggregate/window calls — a bare trailing identifier is never treated
/// as an alias here, matching the existing aggregate-call convention).
fn parse_optional_alias(ts: &mut TokenStream) -> Result<Option<String>, ParseError> {
    if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "as") {
            ts.next();
            return Ok(Some(expect_ident(ts, "alias")?));
        }
    }
    Ok(None)
}

/// Parses optional ORDER BY / LIMIT tail for a simple SELECT (or compound).
fn parse_select_tail(
    ts: &mut TokenStream,
    order_by: &mut Vec<OrderBy>,
    limit: &mut Option<u64>,
    order_by_cosine: &mut Option<OrderByCosine>,
) -> Result<(), ParseError> {
    loop {
        match ts.peek() {
            Tok::Ident(kw) if eq_ignore_case(&kw, "order") => {
                ts.next();
                expect_kw(ts, "by")?;
                let mut descending = false;
                let col = match ts.next() {
                    Tok::Ident(name) => {
                        if eq_ignore_case(&name, "cosine") {
                            expect_tok(ts, Tok::LParen, "'(' after cosine")?;
                            let column = expect_ident(ts, "embedding column")?;
                            expect_tok(ts, Tok::Comma, "',' in cosine(...)")?;
                            let _param = match ts.next() {
                                Tok::Param => 1,
                                _ => {
                                    return Err(ParseError("expected '?' query vector".to_string()))
                                }
                            };
                            expect_tok(ts, Tok::RParen, "')' after cosine(...)")?;
                            expect_kw(ts, "limit")?;
                            let k = expect_int(ts, "LIMIT k")?;
                            *order_by_cosine = Some(OrderByCosine {
                                column,
                                param: 1,
                                k: k as u64,
                            });
                            continue;
                        }
                        name
                    }
                    _ => return Err(ParseError("expected column name in ORDER BY".to_string())),
                };
                if let Tok::Ident(kw) = ts.peek() {
                    if eq_ignore_case(&kw, "desc") {
                        descending = true;
                        ts.next();
                    } else if eq_ignore_case(&kw, "asc") {
                        ts.next();
                    }
                }
                order_by.push(OrderBy {
                    column: col,
                    descending,
                });
                loop {
                    let sp = ts.save();
                    let next = ts.next();
                    if let Tok::Comma = next {
                        match ts.next() {
                            Tok::Ident(name) => {
                                let mut desc = false;
                                if let Tok::Ident(kw) = ts.peek() {
                                    if eq_ignore_case(&kw, "desc") {
                                        desc = true;
                                        ts.next();
                                    } else if eq_ignore_case(&kw, "asc") {
                                        ts.next();
                                    }
                                }
                                order_by.push(OrderBy {
                                    column: name,
                                    descending: desc,
                                });
                            }
                            _ => {
                                return Err(ParseError(
                                    "expected column name in ORDER BY".to_string(),
                                ))
                            }
                        }
                    } else {
                        ts.restore(sp);
                        break;
                    }
                }
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "limit") => {
                ts.next();
                if limit.is_some() {
                    return Err(ParseError("duplicate LIMIT".to_string()));
                }
                match ts.next() {
                    Tok::Int(v) if v >= 0 => *limit = Some(v as u64),
                    _ => return Err(ParseError("expected non-negative LIMIT".to_string())),
                }
            }
            _ => break,
        }
    }
    Ok(())
}

/// Rewrites `Expr::AggCmp`/bare-truthy `Expr::Agg` nodes inside a parsed
/// `HAVING` tree to reference the actual alias the matching SELECT-list
/// aggregate got (an explicit `AS`, or the same default this function falls
/// back to), instead of the aggregate call blindly recomputing a default
/// alias a second time — which breaks the moment the SELECT list uses an
/// explicit alias for that same aggregate. Only recurses through
/// `And`/`Or`/`Not`; does not descend into `Exists`/`InSubquery`/`ScalarCmp`,
/// which have their own, independent aggregate scope. An aggregate in
/// HAVING with no SELECT-list counterpart keeps today's default-alias
/// fallback (materializing HAVING-only aggregates that aren't also
/// projected is out of scope here).
fn resolve_having_aliases(expr: &mut Expr, aggregates: &[(String, AggregateFunc, String)]) {
    fn resolve_alias(
        func: AggregateFunc,
        column: &str,
        aggregates: &[(String, AggregateFunc, String)],
    ) -> String {
        aggregates
            .iter()
            .find(|(_, f, c)| *f == func && c == column)
            .map(|(alias, _, _)| alias.clone())
            .unwrap_or_else(|| crate::executor::agg_default_alias(func, column))
    }

    match expr {
        Expr::And(l, r) | Expr::Or(l, r) => {
            resolve_having_aliases(l, aggregates);
            resolve_having_aliases(r, aggregates);
        }
        Expr::Not(inner) => resolve_having_aliases(inner, aggregates),
        Expr::AggCmp {
            func,
            column,
            op,
            value,
        } => {
            let column = resolve_alias(*func, column, aggregates);
            *expr = Expr::Cmp {
                column,
                op: *op,
                value: value.clone(),
            };
        }
        Expr::Agg { func, column } => {
            let column = resolve_alias(*func, column, aggregates);
            *expr = Expr::IsNull {
                column,
                negated: true,
            };
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// WITH clause
// ---------------------------------------------------------------------------

pub(super) fn parse_with_clause(
    ts: &mut TokenStream,
    is_recursive: bool,
) -> Result<Vec<CTE>, ParseError> {
    let mut ctes = Vec::new();
    loop {
        let name = expect_ident(ts, "CTE name")?;
        expect_kw(ts, "as")?;
        expect_tok(ts, Tok::LParen, "'(' after AS in CTE")?;
        match ts.next() {
            Tok::Ident(kw) if eq_ignore_case(&kw, "select") => {}
            _ => return Err(ParseError("expected SELECT in CTE".to_string())),
        }
        let cte = if is_recursive {
            parse_recursive_cte_body(ts, &name)?
        } else {
            let q = parse_select_inner(ts)?;
            CTE {
                name: name.clone(),
                query: q,
                recursive_term: None,
            }
        };
        expect_tok(ts, Tok::RParen, "')'")?;
        ctes.push(cte);
        match ts.peek() {
            Tok::Comma => {
                ts.next();
                continue;
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "select") => break,
            _ => return Err(ParseError("expected ',' or SELECT after CTE".to_string())),
        }
    }
    Ok(ctes)
}

/// Parses a `WITH RECURSIVE` CTE body: either a plain `SELECT` (a CTE that
/// happens not to self-reference is legal inside a `WITH RECURSIVE` clause)
/// or `anchor UNION [ALL] recursive-term`, where `recursive-term` is expected
/// to reference `name` in its own `FROM`/`JOIN` (not enforced here — an
/// accidentally-non-recursive "recursive" term is just a very expensive
/// constant, not a parse error). Only a single `UNION`/`UNION ALL` between
/// exactly two "select core"s is supported; `INTERSECT`/`EXCEPT`, or more than
/// one set operation, are rejected — Postgres itself only allows
/// `UNION`/`UNION ALL` between a recursive CTE's anchor and recursive term.
fn parse_recursive_cte_body(ts: &mut TokenStream, name: &str) -> Result<CTE, ParseError> {
    ts.enter_depth()?;
    let first = match parse_select_core(ts) {
        Ok(f) => f,
        Err(e) => {
            ts.exit_depth();
            return Err(e);
        }
    };

    let sp = ts.save();
    let op = match ts.peek() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "union") => {
            ts.next();
            let all = if let Tok::Ident(kw2) = ts.peek() {
                if eq_ignore_case(&kw2, "all") {
                    ts.next();
                    true
                } else {
                    false
                }
            } else {
                false
            };
            Some(SetOperation::Union(all))
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "intersect") || eq_ignore_case(&kw, "except") => {
            ts.exit_depth();
            return Err(ParseError(
                "recursive CTE must use UNION or UNION ALL between the anchor and the \
                 recursive term"
                    .to_string(),
            ));
        }
        _ => {
            ts.restore(sp);
            None
        }
    };

    let result = if let Some(op) = op {
        if !matches!(ts.next(), Tok::Ident(kw) if eq_ignore_case(&kw, "select")) {
            ts.exit_depth();
            return Err(ParseError(
                "expected SELECT after UNION in recursive CTE".to_string(),
            ));
        }
        let recursive_term = match parse_select_core(ts) {
            Ok(r) => r,
            Err(e) => {
                ts.exit_depth();
                return Err(e);
            }
        };
        let sp2 = ts.save();
        let has_more = matches!(
            ts.peek(),
            Tok::Ident(kw)
                if eq_ignore_case(&kw, "union")
                    || eq_ignore_case(&kw, "intersect")
                    || eq_ignore_case(&kw, "except")
        );
        ts.restore(sp2);
        if has_more {
            ts.exit_depth();
            return Err(ParseError(
                "recursive CTE supports exactly one UNION between the anchor and the \
                 recursive term"
                    .to_string(),
            ));
        }
        Ok(CTE {
            name: name.to_string(),
            query: first,
            recursive_term: Some((op, Box::new(recursive_term))),
        })
    } else {
        let mut stmt = first;
        let mut obc = stmt.order_by_cosine;
        let tail = parse_select_tail(ts, &mut stmt.order_by, &mut stmt.limit, &mut obc);
        if let Err(e) = tail {
            ts.exit_depth();
            return Err(e);
        }
        stmt.order_by_cosine = obc;
        Ok(CTE {
            name: name.to_string(),
            query: stmt,
            recursive_term: None,
        })
    };
    ts.exit_depth();
    result
}

// ---------------------------------------------------------------------------
// Table reference
// ---------------------------------------------------------------------------

fn parse_table_ref(ts: &mut TokenStream) -> Result<TableRef, ParseError> {
    match ts.next() {
        Tok::LParen => {
            match ts.next() {
                Tok::Ident(kw) if eq_ignore_case(&kw, "select") => {}
                _ => return Err(ParseError("expected SELECT in subquery".to_string())),
            }
            let query = parse_select_inner(ts)?;
            expect_tok(ts, Tok::RParen, "')' after subquery")?;
            let alias = match ts.next() {
                Tok::Ident(kw) if eq_ignore_case(&kw, "as") => expect_ident(ts, "subquery alias")?,
                _ => return Err(ParseError("subquery must have an AS alias".to_string())),
            };
            Ok(TableRef::Subquery {
                query: Box::new(query),
                alias,
            })
        }
        Tok::Ident(name) => {
            let alias = if let Tok::Ident(kw) = ts.peek() {
                if eq_ignore_case(&kw, "as") {
                    ts.next();
                    Some(expect_ident(ts, "table alias")?)
                } else {
                    None
                }
            } else {
                None
            };
            Ok(TableRef::Named {
                name: name.to_string(),
                alias,
            })
        }
        _ => Err(ParseError("expected table name or '('".to_string())),
    }
}
