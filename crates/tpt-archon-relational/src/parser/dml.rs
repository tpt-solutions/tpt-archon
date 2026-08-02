//! DML statement parsing: `INSERT`, `UPDATE`, `DELETE`.

use alloc::string::ToString;
use alloc::vec::Vec;

use super::ast::{
    Assignment, CmpOp, DeleteStatement, InsertStatement, ParseError, UpdateStatement,
};
use super::expr::{parse_expr, parse_literal};
use super::lexer::{eq_ignore_case, expect_ident, expect_kw, expect_tok, Tok, TokenStream};

// ---------------------------------------------------------------------------
// INSERT
// ---------------------------------------------------------------------------

pub(super) fn parse_insert(ts: &mut TokenStream) -> Result<InsertStatement, ParseError> {
    expect_kw(ts, "into")?;
    let table = expect_ident(ts, "table name")?;
    let mut columns = Vec::new();
    if let Tok::LParen = ts.peek() {
        ts.next();
        loop {
            let col = expect_ident(ts, "column name")?;
            columns.push(col);
            match ts.next() {
                Tok::Comma => continue,
                Tok::RParen => break,
                _ => return Err(ParseError("expected ',' or ')'".to_string())),
            }
        }
    }
    expect_kw(ts, "values")?;
    expect_tok(ts, Tok::LParen, "'(' before values")?;
    let mut values = Vec::new();
    loop {
        values.push(parse_literal(ts)?);
        match ts.next() {
            Tok::Comma => continue,
            Tok::RParen => break,
            _ => return Err(ParseError("expected ',' or ')'".to_string())),
        }
    }
    if !matches!(ts.next(), Tok::Eof) {
        return Err(ParseError("trailing tokens after statement".to_string()));
    }
    Ok(InsertStatement {
        table,
        columns,
        values,
    })
}

// ---------------------------------------------------------------------------
// UPDATE
// ---------------------------------------------------------------------------

pub(super) fn parse_update(ts: &mut TokenStream) -> Result<UpdateStatement, ParseError> {
    let table = expect_ident(ts, "table name")?;
    expect_kw(ts, "set")?;
    let mut assignments = Vec::new();
    loop {
        let column = expect_ident(ts, "column name")?;
        expect_tok(ts, Tok::Op(CmpOp::Eq), "'=' in assignment")?;
        let value = parse_literal(ts)?;
        assignments.push(Assignment { column, value });
        if let Tok::Comma = ts.peek() {
            ts.next();
            continue;
        }
        break;
    }
    let filter = if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "where") {
            ts.next();
            Some(parse_expr(ts)?)
        } else {
            None
        }
    } else {
        None
    };
    if !matches!(ts.next(), Tok::Eof) {
        return Err(ParseError("trailing tokens".to_string()));
    }
    Ok(UpdateStatement {
        table,
        assignments,
        filter,
    })
}

// ---------------------------------------------------------------------------
// DELETE
// ---------------------------------------------------------------------------

pub(super) fn parse_delete(ts: &mut TokenStream) -> Result<DeleteStatement, ParseError> {
    expect_kw(ts, "from")?;
    let table = expect_ident(ts, "table name")?;
    let filter = if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "where") {
            ts.next();
            Some(parse_expr(ts)?)
        } else {
            None
        }
    } else {
        None
    };
    if !matches!(ts.next(), Tok::Eof) {
        return Err(ParseError("trailing tokens after statement".to_string()));
    }
    Ok(DeleteStatement { table, filter })
}
