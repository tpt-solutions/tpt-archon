//! DDL statement parsing: `CREATE VIEW`, `CREATE TABLE`, `ALTER TABLE`.

use alloc::string::ToString;
use alloc::vec::Vec;

use super::ast::{
    AlterTableOp, AlterTableStatement, ColumnDef, ColumnType, CreateTableStatement,
    CreateViewStatement, ParseError,
};
use super::lexer::{
    eq_ignore_case, expect_ident, expect_int, expect_kw, expect_tok, Tok, TokenStream,
};
use super::select::parse_select_inner;

// ---------------------------------------------------------------------------
// CREATE VIEW
// ---------------------------------------------------------------------------

pub(super) fn parse_create_view(ts: &mut TokenStream) -> Result<CreateViewStatement, ParseError> {
    let name = expect_ident(ts, "view name")?;
    expect_kw(ts, "as")?;
    match ts.next() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "select") => {
            let query = parse_select_inner(ts)?;
            if !matches!(ts.next(), Tok::Eof) {
                return Err(ParseError("trailing tokens after statement".to_string()));
            }
            Ok(CreateViewStatement { name, query })
        }
        _ => Err(ParseError(
            "expected SELECT after CREATE VIEW ... AS".to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// CREATE TABLE
// ---------------------------------------------------------------------------

pub(super) fn parse_create_table(ts: &mut TokenStream) -> Result<CreateTableStatement, ParseError> {
    expect_kw(ts, "table")?;
    let table = expect_ident(ts, "table name")?;
    expect_tok(ts, Tok::LParen, "'('")?;
    let mut columns = Vec::new();
    loop {
        let name = expect_ident(ts, "column name")?;
        let ctype = match ts.next() {
            Tok::Ident(kw) if eq_ignore_case(&kw, "int") || eq_ignore_case(&kw, "integer") => {
                ColumnType::Int
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "boolean") => ColumnType::Boolean,
            Tok::Ident(kw) if eq_ignore_case(&kw, "float") => ColumnType::Float,
            Tok::Ident(kw) if eq_ignore_case(&kw, "double") => ColumnType::Double,
            Tok::Ident(kw) if eq_ignore_case(&kw, "numeric") => ColumnType::Numeric,
            Tok::Ident(kw) if eq_ignore_case(&kw, "text") => ColumnType::Text,
            Tok::Ident(kw) if eq_ignore_case(&kw, "varchar") => {
                let len = if let Tok::LParen = ts.peek() {
                    ts.next();
                    let n = expect_int(ts, "VARCHAR length")?;
                    expect_tok(ts, Tok::RParen, "')' after VARCHAR length")?;
                    n as usize
                } else {
                    255
                };
                ColumnType::Varchar(len)
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "date") => ColumnType::Date,
            Tok::Ident(kw) if eq_ignore_case(&kw, "timestamp") => ColumnType::Timestamp,
            Tok::Ident(kw) if eq_ignore_case(&kw, "vector") => {
                if let Tok::LBracket = ts.peek() {
                    ts.next();
                    loop {
                        match ts.peek() {
                            Tok::RBracket => {
                                ts.next();
                                break;
                            }
                            Tok::Int(_) | Tok::Float(_) | Tok::Comma => {
                                ts.next();
                            }
                            _ => {
                                return Err(ParseError(
                                    "expected ']' after VECTOR dimension".to_string(),
                                ))
                            }
                        }
                    }
                }
                ColumnType::Vector
            }
            _ => {
                return Err(ParseError(
                    "expected column type (INT, TEXT, VECTOR, ...)".to_string(),
                ))
            }
        };
        columns.push(ColumnDef { name, ctype });
        match ts.next() {
            Tok::Comma => continue,
            Tok::RParen => break,
            _ => return Err(ParseError("expected ',' or ')'".to_string())),
        }
    }
    Ok(CreateTableStatement { table, columns })
}

// ---------------------------------------------------------------------------
// ALTER TABLE
// ---------------------------------------------------------------------------

pub(super) fn parse_alter_table(ts: &mut TokenStream) -> Result<AlterTableStatement, ParseError> {
    let table = expect_ident(ts, "table name")?;
    let op = match ts.next() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "add") => {
            expect_kw(ts, "column")?;
            let name = expect_ident(ts, "column name")?;
            let ctype = match ts.next() {
                Tok::Ident(kw) if eq_ignore_case(&kw, "int") || eq_ignore_case(&kw, "integer") => {
                    ColumnType::Int
                }
                Tok::Ident(kw) if eq_ignore_case(&kw, "boolean") => ColumnType::Boolean,
                Tok::Ident(kw) if eq_ignore_case(&kw, "float") => ColumnType::Float,
                Tok::Ident(kw) if eq_ignore_case(&kw, "double") => ColumnType::Double,
                Tok::Ident(kw) if eq_ignore_case(&kw, "numeric") => ColumnType::Numeric,
                Tok::Ident(kw) if eq_ignore_case(&kw, "text") => ColumnType::Text,
                Tok::Ident(kw) if eq_ignore_case(&kw, "varchar") => {
                    let len = if let Tok::LParen = ts.peek() {
                        ts.next();
                        let n = expect_int(ts, "VARCHAR length")?;
                        expect_tok(ts, Tok::RParen, "')' after VARCHAR length")?;
                        n as usize
                    } else {
                        255
                    };
                    ColumnType::Varchar(len)
                }
                Tok::Ident(kw) if eq_ignore_case(&kw, "date") => ColumnType::Date,
                Tok::Ident(kw) if eq_ignore_case(&kw, "timestamp") => ColumnType::Timestamp,
                Tok::Ident(kw) if eq_ignore_case(&kw, "vector") => {
                    if let Tok::LBracket = ts.peek() {
                        ts.next();
                        loop {
                            match ts.peek() {
                                Tok::RBracket => {
                                    ts.next();
                                    break;
                                }
                                Tok::Int(_) | Tok::Float(_) | Tok::Comma => {
                                    ts.next();
                                }
                                _ => {
                                    return Err(ParseError(
                                        "expected ']' after VECTOR dimension".to_string(),
                                    ))
                                }
                            }
                        }
                    }
                    ColumnType::Vector
                }
                _ => {
                    return Err(ParseError(
                        "expected column type (INT, TEXT, VECTOR, ...)".to_string(),
                    ))
                }
            };
            AlterTableOp::AddColumn(ColumnDef { name, ctype })
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "drop") => {
            expect_kw(ts, "column")?;
            let name = expect_ident(ts, "column name")?;
            AlterTableOp::DropColumn(name)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "rename") => {
            expect_kw(ts, "column")?;
            let old_name = expect_ident(ts, "old column name")?;
            expect_kw(ts, "to")?;
            let new_name = expect_ident(ts, "new column name")?;
            AlterTableOp::RenameColumn { old_name, new_name }
        }
        _ => {
            return Err(ParseError(
                "expected ADD, DROP, or RENAME after ALTER TABLE".to_string(),
            ))
        }
    };
    Ok(AlterTableStatement { table, op })
}
