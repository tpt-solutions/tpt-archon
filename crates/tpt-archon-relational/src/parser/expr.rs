//! Expression parsing (`WHERE`/`HAVING`): the `AND`/`OR`/`NOT` precedence
//! chain down to comparisons, `IS [NOT] NULL`, `LIKE`, `IN`, `BETWEEN`,
//! `EXISTS`, and subquery comparisons.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;

use super::ast::{AggregateFunc, CmpOp, Expr, Literal, ParseError};
use super::lexer::{
    eq_ignore_case, expect_ident, expect_int, expect_kw, expect_tok, is_kw, Tok, TokenStream,
};
use super::select::parse_select_inner;

pub(super) fn parse_expr(ts: &mut TokenStream) -> Result<Expr, ParseError> {
    // Depth is guarded once per recursive-descent level in `parse_primary_expr`
    // (the only caller of which is, transitively, this function) — guarding
    // here too would double-count every paren/subquery nesting level.
    parse_or(ts)
}

fn parse_or(ts: &mut TokenStream) -> Result<Expr, ParseError> {
    let mut left = parse_and(ts)?;
    loop {
        if let Tok::Ident(kw) = ts.peek() {
            if eq_ignore_case(&kw, "or") {
                ts.next();
                let right = parse_and(ts)?;
                left = Expr::Or(Box::new(left), Box::new(right));
                continue;
            }
        }
        break;
    }
    Ok(left)
}

fn parse_and(ts: &mut TokenStream) -> Result<Expr, ParseError> {
    let mut left = parse_not(ts)?;
    loop {
        if let Tok::Ident(kw) = ts.peek() {
            if eq_ignore_case(&kw, "and") {
                ts.next();
                let right = parse_not(ts)?;
                left = Expr::And(Box::new(left), Box::new(right));
                continue;
            }
        }
        break;
    }
    Ok(left)
}

fn parse_not(ts: &mut TokenStream) -> Result<Expr, ParseError> {
    if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "not") {
            ts.next();
            ts.enter_depth()?;
            let inner = parse_not(ts);
            ts.exit_depth();
            return Ok(Expr::Not(Box::new(inner?)));
        }
    }
    parse_primary_expr(ts)
}

fn parse_primary_expr(ts: &mut TokenStream) -> Result<Expr, ParseError> {
    ts.enter_depth()?;
    let result = match ts.peek() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "exists") => {
            ts.next();
            expect_tok(ts, Tok::LParen, "'(' after EXISTS")?;
            expect_kw(ts, "select")?;
            let query = parse_select_inner(ts)?;
            expect_tok(ts, Tok::RParen, "')' after EXISTS subquery")?;
            Ok(Expr::Exists {
                query: Box::new(query),
            })
        }
        Tok::Ident(kw)
            if matches!(
                kw.to_ascii_lowercase().as_str(),
                "count" | "sum" | "avg" | "min" | "max"
            ) =>
        {
            let func = match kw.to_ascii_lowercase().as_str() {
                "count" => AggregateFunc::Count,
                "sum" => AggregateFunc::Sum,
                "avg" => AggregateFunc::Avg,
                "min" => AggregateFunc::Min,
                "max" => AggregateFunc::Max,
                _ => unreachable!(),
            };
            ts.next();
            expect_tok(ts, Tok::LParen, "'(' after aggregate function")?;
            let col = if let Tok::Star = ts.peek() {
                ts.next();
                expect_tok(ts, Tok::RParen, "')'")?;
                "*".to_string()
            } else {
                let c = expect_ident(ts, "column in aggregate")?;
                expect_tok(ts, Tok::RParen, "')'")?;
                c
            };
            match ts.peek() {
                Tok::Op(op) => {
                    ts.next();
                    let value = parse_literal(ts)?;
                    Ok(Expr::AggCmp {
                        func,
                        column: col,
                        op,
                        value,
                    })
                }
                _ => Ok(Expr::Agg { func, column: col }),
            }
        }
        Tok::Ident(col) => {
            ts.next();
            if is_kw(&col) {
                return Err(ParseError(alloc::format!(
                    "unexpected keyword '{col}' in expression"
                )));
            }
            let mut col_name = col.to_string();
            if let Tok::Dot = ts.peek() {
                ts.next();
                let field = expect_ident(ts, "field name after '.'")?;
                col_name = alloc::format!("{col_name}.{field}");
            }
            match ts.peek() {
                Tok::Ident(kw) if eq_ignore_case(&kw, "is") => {
                    ts.next();
                    let negated = match ts.next() {
                        Tok::Ident(kw) if eq_ignore_case(&kw, "not") => {
                            expect_kw(ts, "null")?;
                            true
                        }
                        Tok::Ident(kw) if eq_ignore_case(&kw, "null") => false,
                        _ => {
                            return Err(ParseError(
                                "expected NULL or NOT NULL after IS".to_string(),
                            ))
                        }
                    };
                    Ok(Expr::IsNull {
                        column: col_name,
                        negated,
                    })
                }
                Tok::Ident(kw) if eq_ignore_case(&kw, "like") => {
                    ts.next();
                    let pattern = match ts.next() {
                        Tok::Text(s) => s.to_string(),
                        _ => {
                            return Err(ParseError(
                                "expected pattern string after LIKE".to_string(),
                            ))
                        }
                    };
                    Ok(Expr::Like {
                        column: col_name,
                        pattern,
                    })
                }
                Tok::Ident(kw) if eq_ignore_case(&kw, "between") => {
                    ts.next();
                    let low = expect_int(ts, "BETWEEN low")?;
                    expect_kw(ts, "and")?;
                    let high = expect_int(ts, "BETWEEN high")?;
                    Ok(Expr::BetweenInt {
                        column: col_name,
                        low,
                        high,
                    })
                }
                Tok::Ident(kw) if eq_ignore_case(&kw, "in") => {
                    ts.next();
                    expect_tok(ts, Tok::LParen, "'(' after IN")?;
                    let is_subquery = if let Tok::Ident(k2) = ts.peek() {
                        eq_ignore_case(&k2, "select")
                    } else {
                        false
                    };
                    if is_subquery {
                        ts.next();
                        let query = parse_select_inner(ts)?;
                        expect_tok(ts, Tok::RParen, "')' after IN subquery")?;
                        Ok(Expr::InSubquery {
                            column: col_name,
                            query: Box::new(query),
                        })
                    } else {
                        let mut values = Vec::new();
                        loop {
                            values.push(expect_int(ts, "value in IN list")?);
                            match ts.next() {
                                Tok::Comma => continue,
                                Tok::RParen => break,
                                _ => {
                                    return Err(ParseError(
                                        "expected ',' or ')' in IN list".to_string(),
                                    ))
                                }
                            }
                        }
                        Ok(Expr::InInt {
                            column: col_name,
                            values,
                        })
                    }
                }
                Tok::Op(op) => {
                    ts.next();
                    match ts.peek() {
                        Tok::Ident(kw) if eq_ignore_case(&kw, "null") => {
                            ts.next();
                            match op {
                                CmpOp::Eq => Ok(Expr::IsNull {
                                    column: col_name,
                                    negated: false,
                                }),
                                CmpOp::Ne => Ok(Expr::IsNull {
                                    column: col_name,
                                    negated: true,
                                }),
                                _ => Err(ParseError(
                                    "comparison with NULL requires IS / IS NOT".to_string(),
                                )),
                            }
                        }
                        Tok::Int(v) => {
                            ts.next();
                            Ok(Expr::Cmp {
                                column: col_name,
                                op,
                                value: Literal::Int(v),
                            })
                        }
                        Tok::Float(v) => {
                            ts.next();
                            Ok(Expr::Cmp {
                                column: col_name,
                                op,
                                value: Literal::Float(v),
                            })
                        }
                        Tok::Text(s) => {
                            ts.next();
                            Ok(Expr::Cmp {
                                column: col_name,
                                op,
                                value: Literal::Text(s.to_string()),
                            })
                        }
                        Tok::Ident(right) => {
                            ts.next();
                            let mut right = right.to_string();
                            if let Tok::Dot = ts.peek() {
                                ts.next();
                                let field = expect_ident(ts, "field name after '.'")?;
                                right = alloc::format!("{right}.{field}");
                            }
                            Ok(Expr::CmpColumn {
                                left: col_name,
                                op,
                                right,
                            })
                        }
                        Tok::LParen => {
                            ts.next();
                            expect_kw(ts, "select")?;
                            let query = parse_select_inner(ts)?;
                            expect_tok(ts, Tok::RParen, "')' after scalar subquery")?;
                            Ok(Expr::ScalarCmp {
                                column: col_name,
                                op,
                                query: Box::new(query),
                            })
                        }
                        _ => Err(ParseError("expected value after operator".to_string())),
                    }
                }
                _ => Err(ParseError(
                    "expected operator or IS after column name".to_string(),
                )),
            }
        }
        Tok::LParen => {
            ts.next();
            let inner = parse_expr(ts)?;
            expect_tok(ts, Tok::RParen, "')'")?;
            Ok(inner)
        }
        _ => Err(ParseError(
            "expected column name or '(' in expression".to_string(),
        )),
    };
    ts.exit_depth();
    result
}

fn parse_vector_literal(ts: &mut TokenStream) -> Result<Literal, ParseError> {
    let mut vals = Vec::new();
    loop {
        match ts.peek() {
            Tok::Float(v) => {
                vals.push(v);
                ts.next();
            }
            Tok::Int(v) => {
                vals.push(v as f32);
                ts.next();
            }
            Tok::RBracket => {
                ts.next();
                break;
            }
            _ => {
                return Err(ParseError(
                    "expected number or ']' in vector literal".to_string(),
                ))
            }
        }
        if let Tok::Comma = ts.peek() {
            ts.next();
        }
    }
    Ok(Literal::Vector(vals))
}

pub(super) fn parse_literal(ts: &mut TokenStream) -> Result<Literal, ParseError> {
    match ts.next() {
        Tok::Int(v) => Ok(Literal::Int(v)),
        Tok::Float(v) => Ok(Literal::Float(v)),
        Tok::Text(s) => Ok(Literal::Text(s.to_string())),
        Tok::LBracket => parse_vector_literal(ts),
        Tok::Ident(kw) if eq_ignore_case(&kw, "null") => Ok(Literal::Null),
        _ => Err(ParseError("expected a value".to_string())),
    }
}
