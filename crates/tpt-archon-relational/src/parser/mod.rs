//! A hand-written, allocation-light SQL parser (PostgreSQL-leaning dialect).
//!
//! Covers the subset the executor supports today:
//!
//! ```sql
//! SELECT <col, ...> FROM <table> [JOIN <table> ON <cond>]*
//!   [WHERE <expr>] [GROUP BY <col, ...>] [ORDER BY <col> [ASC|DESC], ...]
//!   [LIMIT <n>]
//! INSERT INTO <table> [(col, ...)] VALUES (v, ...)
//! UPDATE <table> SET col = v, ... [WHERE <expr>]
//! DELETE FROM <table> [WHERE <expr>]
//! CREATE TABLE <table> (col type, ...)
//! BEGIN / COMMIT / ROLLBACK
//! ```
//!
//! `<expr>` supports `AND`/`OR`/`NOT`, `IS [NOT] NULL`, `LIKE`, `IN`,
//! `BETWEEN`, and comparisons against integer, text, float, or `NULL` literals.
//!
//! It uses a pre-tokenized `TokenStream` that borrows directly from the input
//! string during construction, then owns all tokens for the duration of parsing.
//! PostgreSQL compatibility is the target dialect (spec Risk 2: PostgreSQL
//! first, SQLite later); the grammar grows from here.
//!
//! Submodules mirror the grammar: [`ast`] (types), [`lexer`] (tokenizer),
//! [`expr`] (`WHERE`/`HAVING` expressions), [`ddl`] (`CREATE`/`ALTER TABLE`),
//! [`dml`] (`INSERT`/`UPDATE`/`DELETE`), and [`select`] (`SELECT`/`WITH`).
//! Only the two entry points below and the [`ast`] types are part of the
//! crate's public surface; everything else is `pub(super)` so it can be
//! shared across these submodules without leaking outside `parser`.

use alloc::string::ToString;
use alloc::vec::Vec;

mod ast;
mod ddl;
mod dml;
mod expr;
mod lexer;
mod select;
#[cfg(test)]
mod tests;

pub use ast::*;

use ddl::{parse_alter_table, parse_create_table, parse_create_view};
use dml::{parse_delete, parse_insert, parse_update};
use lexer::{eq_ignore_case, expect_ident, expect_kw, Tok, TokenStream};
use select::{
    parse_select_inner, parse_select_or_compound, parse_with_clause, try_parse_select_literal,
};

pub fn parse_statement(input: &str) -> Result<Statement, ParseError> {
    let mut ts = TokenStream::new(input)?;
    match ts.next() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "with") => {
            let is_recursive = if let Tok::Ident(kw2) = ts.peek() {
                if eq_ignore_case(&kw2, "recursive") {
                    ts.next();
                    true
                } else {
                    false
                }
            } else {
                false
            };
            let ctes = parse_with_clause(&mut ts, is_recursive)?;
            match ts.next() {
                Tok::Ident(kw) if eq_ignore_case(&kw, "select") => {}
                _ => return Err(ParseError("expected SELECT after WITH".to_string())),
            }
            let stmt = parse_select_or_compound(&mut ts, ctes)?;
            if !matches!(ts.next(), Tok::Eof) {
                return Err(ParseError("trailing tokens after statement".to_string()));
            }
            Ok(stmt)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "select") => {
            if let Some(items) = try_parse_select_literal(&mut ts) {
                // try_parse_select_literal already confirmed Eof follows.
                ts.next();
                return Ok(Statement::SelectLiteral(items));
            }
            let stmt = parse_select_or_compound(&mut ts, Vec::new())?;
            if !matches!(ts.next(), Tok::Eof) {
                return Err(ParseError("trailing tokens after statement".to_string()));
            }
            Ok(stmt)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "insert") => {
            parse_insert(&mut ts).map(Statement::Insert)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "update") => {
            parse_update(&mut ts).map(Statement::Update)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "delete") => {
            parse_delete(&mut ts).map(Statement::Delete)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "create") => match ts.peek() {
            Tok::Ident(s) if eq_ignore_case(&s, "table") => {
                parse_create_table(&mut ts).map(Statement::CreateTable)
            }
            Tok::Ident(s) if eq_ignore_case(&s, "view") => {
                ts.next();
                parse_create_view(&mut ts).map(Statement::CreateView)
            }
            _ => Err(ParseError(
                "expected TABLE or VIEW after CREATE".to_string(),
            )),
        },
        Tok::Ident(kw) if eq_ignore_case(&kw, "drop") => match ts.next() {
            Tok::Ident(s) if eq_ignore_case(&s, "view") => {
                let name = expect_ident(&mut ts, "view name")?;
                if !matches!(ts.next(), Tok::Eof) {
                    return Err(ParseError("trailing tokens after statement".to_string()));
                }
                Ok(Statement::DropView(name))
            }
            _ => Err(ParseError("expected VIEW after DROP".to_string())),
        },
        Tok::Ident(kw) if eq_ignore_case(&kw, "alter") => {
            expect_kw(&mut ts, "table")?;
            let stmt = parse_alter_table(&mut ts)?;
            if !matches!(ts.next(), Tok::Eof) {
                return Err(ParseError("trailing tokens after statement".to_string()));
            }
            Ok(Statement::AlterTable(stmt))
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "begin") => Ok(Statement::Begin),
        Tok::Ident(kw) if eq_ignore_case(&kw, "commit") => Ok(Statement::Commit),
        Tok::Ident(kw) if eq_ignore_case(&kw, "rollback") => Ok(Statement::Rollback),
        Tok::Ident(kw) if eq_ignore_case(&kw, "set") => {
            parse_set(&mut ts).map(Statement::SetParameter)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "show") => {
            parse_show(&mut ts).map(Statement::ShowParameter)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "reset") => parse_reset(&mut ts),
        _ => Err(ParseError(
            "expected SELECT, INSERT, UPDATE, DELETE, CREATE TABLE, CREATE VIEW, DROP VIEW, \
             ALTER TABLE, BEGIN, COMMIT, ROLLBACK, SET, SHOW, or RESET"
                .to_string(),
        )),
    }
}

/// Parse `SET parameter = value`
fn parse_set(ts: &mut TokenStream) -> Result<SetParameterStatement, ParseError> {
    let name = expect_ident(ts, "parameter name")?;
    expect_kw(ts, "=")?;
    // Value can be an identifier, string, or number - read as raw token
    let value = match ts.next() {
        Tok::Ident(s) => s,
        Tok::Text(s) => s,
        Tok::Int(i) => i.to_string(),
        Tok::Float(f) => f.to_string(),
        _ => return Err(ParseError("expected parameter value after =".to_string())),
    };
    if !matches!(ts.next(), Tok::Eof) {
        return Err(ParseError(
            "trailing tokens after SET statement".to_string(),
        ));
    }
    Ok(SetParameterStatement { name, value })
}

/// Parse `SHOW parameter`
fn parse_show(ts: &mut TokenStream) -> Result<ShowParameterStatement, ParseError> {
    let name = expect_ident(ts, "parameter name")?;
    if !matches!(ts.next(), Tok::Eof) {
        return Err(ParseError(
            "trailing tokens after SHOW statement".to_string(),
        ));
    }
    Ok(ShowParameterStatement { name })
}

/// Parse `RESET parameter` or `RESET ALL`
fn parse_reset(ts: &mut TokenStream) -> Result<Statement, ParseError> {
    match ts.peek() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "all") => {
            ts.next(); // consume ALL
            if !matches!(ts.next(), Tok::Eof) {
                return Err(ParseError("trailing tokens after RESET ALL".to_string()));
            }
            Ok(Statement::ResetAll(ResetAllStatement))
        }
        _ => {
            let name = expect_ident(ts, "parameter name")?;
            if !matches!(ts.next(), Tok::Eof) {
                return Err(ParseError(
                    "trailing tokens after RESET statement".to_string(),
                ));
            }
            Ok(Statement::ResetParameter(ResetParameterStatement { name }))
        }
    }
}

/// Convenience: parse a standalone SELECT (with optional WITH clause).
pub fn parse_select(input: &str) -> Result<SelectStatement, ParseError> {
    let mut ts = TokenStream::new(input)?;
    let ctes = if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "with") {
            ts.next();
            let is_recursive = if let Tok::Ident(kw2) = ts.peek() {
                if eq_ignore_case(&kw2, "recursive") {
                    ts.next();
                    true
                } else {
                    false
                }
            } else {
                false
            };
            parse_with_clause(&mut ts, is_recursive)?
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    match ts.next() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "select") => {}
        _ => return Err(ParseError("expected SELECT".to_string())),
    }
    let mut stmt = parse_select_inner(&mut ts)?;
    stmt.with_ctes = ctes;
    if !matches!(ts.next(), Tok::Eof) {
        return Err(ParseError("trailing tokens after statement".to_string()));
    }
    Ok(stmt)
}
