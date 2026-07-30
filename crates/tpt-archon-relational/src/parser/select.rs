//! `SELECT` parsing: columns/aggregates, `FROM`/`JOIN`, `WHERE`/`GROUP BY`/
//! `HAVING`/`ORDER BY`/`LIMIT`, plus the `WITH` clause and table-reference
//! parsing (including derived-table subqueries) that assemble a `SELECT`'s
//! sources.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::ast::{
    AggregateFunc, CmpOp, Expr, Join, JoinType, OrderBy, OrderByCosine, ParseError,
    SelectStatement, TableRef, CTE,
};
use super::expr::parse_expr;
use super::lexer::{
    eq_ignore_case, expect_ident, expect_int, expect_kw, expect_tok, Tok, TokenStream,
};

// ---------------------------------------------------------------------------
// SELECT
// ---------------------------------------------------------------------------

pub(super) fn parse_select_inner(ts: &mut TokenStream) -> Result<SelectStatement, ParseError> {
    ts.enter_depth()?;
    let result = parse_select_inner_impl(ts);
    ts.exit_depth();
    result
}

fn parse_select_inner_impl(ts: &mut TokenStream) -> Result<SelectStatement, ParseError> {
    let mut columns = Vec::new();
    let mut star = false;
    let mut aggregates = Vec::new();

    if let Tok::Star = ts.peek() {
        star = true;
        ts.next();
    } else {
        loop {
            match ts.next() {
                Tok::Ident(name) => {
                    let col_name = name.clone();
                    let agg_func = match col_name.to_ascii_lowercase().as_str() {
                        "count" => Some(AggregateFunc::Count),
                        "sum" => Some(AggregateFunc::Sum),
                        "avg" => Some(AggregateFunc::Avg),
                        "min" => Some(AggregateFunc::Min),
                        "max" => Some(AggregateFunc::Max),
                        _ => None,
                    };
                    // Only treat this identifier as an aggregate call if a '('
                    // actually follows — otherwise it's a plain column that
                    // happens to share a name with an aggregate function
                    // (e.g. a `count` column), which must still parse.
                    if let Some(agg) = agg_func.filter(|_| matches!(ts.peek(), Tok::LParen)) {
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
                        let alias = if let Tok::Ident(kw) = ts.peek() {
                            if eq_ignore_case(&kw, "as") {
                                ts.next();
                                expect_ident(ts, "alias")?
                            } else {
                                crate::executor::agg_default_alias(agg, &inner)
                            }
                        } else {
                            crate::executor::agg_default_alias(agg, &inner)
                        };
                        aggregates.push((alias.clone(), agg, inner));
                        columns.push(alias);
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
        if let Tok::Ident(kw) = ts.peek() {
            if eq_ignore_case(&kw, "join") {
                ts.next();
                let join_table = parse_table_ref(ts)?;
                expect_kw(ts, "on")?;
                let left_col = expect_ident(ts, "left column")?;
                expect_tok(ts, Tok::Op(CmpOp::Eq), "'=' in ON clause")?;
                let right_col = expect_ident(ts, "right column")?;
                joins.push(Join {
                    jtype: JoinType::Inner,
                    table: join_table,
                    left_col,
                    right_col,
                });
                continue;
            }
        }
        break;
    }

    let mut filter = None;
    let mut limit = None;
    let mut order_by_cosine = None;
    let mut group_by = Vec::new();
    let mut having = None;
    let mut order_by = Vec::new();

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
                            order_by_cosine = Some(OrderByCosine {
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
                    Tok::Int(v) if v >= 0 => limit = Some(v as u64),
                    _ => return Err(ParseError("expected non-negative LIMIT".to_string())),
                }
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
        order_by,
        order_by_cosine,
        limit,
        having,
        with_ctes: Vec::new(),
    })
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

pub(super) fn parse_with_clause(ts: &mut TokenStream) -> Result<Vec<CTE>, ParseError> {
    let mut ctes = Vec::new();
    loop {
        let name = expect_ident(ts, "CTE name")?;
        expect_kw(ts, "as")?;
        expect_tok(ts, Tok::LParen, "'(' after AS in CTE")?;
        match ts.next() {
            Tok::Ident(kw) if eq_ignore_case(&kw, "select") => {}
            _ => return Err(ParseError("expected SELECT in CTE".to_string())),
        }
        let q = parse_select_inner(ts)?;
        expect_tok(ts, Tok::RParen, "')'")?;
        ctes.push(CTE {
            name: name.clone(),
            query: q,
        });
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
