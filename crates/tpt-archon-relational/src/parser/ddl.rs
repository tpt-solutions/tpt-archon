//! DDL statement parsing: `CREATE VIEW`, `CREATE TABLE`, `ALTER TABLE`.

use alloc::string::ToString;
use alloc::vec::Vec;

use super::ast::{
    AlterTableOp, AlterTableStatement, ColumnConstraint, ColumnDef, ColumnType,
    CreateTableStatement, CreateViewStatement, ParseError, TableConstraint,
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

/// Parses the type of a column (shared by `CREATE TABLE` and `ALTER TABLE ADD
/// COLUMN`). Returns the [`ColumnType`].
fn parse_column_type(ts: &mut TokenStream) -> Result<ColumnType, ParseError> {
    match ts.next() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "int") || eq_ignore_case(&kw, "integer") => {
            Ok(ColumnType::Int)
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "boolean") => Ok(ColumnType::Boolean),
        Tok::Ident(kw) if eq_ignore_case(&kw, "float") => Ok(ColumnType::Float),
        Tok::Ident(kw) if eq_ignore_case(&kw, "double") => Ok(ColumnType::Double),
        Tok::Ident(kw) if eq_ignore_case(&kw, "numeric") => Ok(ColumnType::Numeric),
        Tok::Ident(kw) if eq_ignore_case(&kw, "text") => Ok(ColumnType::Text),
        Tok::Ident(kw) if eq_ignore_case(&kw, "varchar") => {
            let len = if let Tok::LParen = ts.peek() {
                ts.next();
                let n = expect_int(ts, "VARCHAR length")?;
                expect_tok(ts, Tok::RParen, "')' after VARCHAR length")?;
                n as usize
            } else {
                255
            };
            Ok(ColumnType::Varchar(len))
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "date") => Ok(ColumnType::Date),
        Tok::Ident(kw) if eq_ignore_case(&kw, "timestamp") => Ok(ColumnType::Timestamp),
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
            Ok(ColumnType::Vector)
        }
        _ => Err(ParseError(
            "expected column type (INT, TEXT, VECTOR, ...)".to_string(),
        )),
    }
}

/// Parses zero or more column-level constraints following a column's type:
/// `NOT NULL`, `PRIMARY KEY`, `UNIQUE`.
fn parse_column_constraints(ts: &mut TokenStream) -> Result<Vec<ColumnConstraint>, ParseError> {
    let mut out = Vec::new();
    loop {
        match ts.peek() {
            Tok::Ident(kw) if eq_ignore_case(&kw, "not") => {
                ts.next();
                expect_kw(ts, "null")?;
                out.push(ColumnConstraint::NotNull);
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "primary") => {
                ts.next();
                expect_kw(ts, "key")?;
                out.push(ColumnConstraint::PrimaryKey);
            }
            Tok::Ident(kw) if eq_ignore_case(&kw, "unique") => {
                ts.next();
                out.push(ColumnConstraint::Unique);
            }
            _ => break,
        }
    }
    Ok(out)
}

/// Whether the next tokens begin a table-level constraint rather than a column
/// definition: `PRIMARY KEY`, `UNIQUE`, `FOREIGN KEY`, `CHECK`, or a leading
/// `CONSTRAINT <name>`.
fn is_table_constraint(ts: &TokenStream) -> bool {
    matches!(
        ts.peek(),
        Tok::Ident(kw)
            if eq_ignore_case(&kw, "primary")
                || eq_ignore_case(&kw, "unique")
                || eq_ignore_case(&kw, "foreign")
                || eq_ignore_case(&kw, "check")
                || eq_ignore_case(&kw, "constraint")
    )
}

/// Parses a single table-level constraint.
fn parse_table_constraint(ts: &mut TokenStream) -> Result<TableConstraint, ParseError> {
    // Optional leading `CONSTRAINT <name>` — the name is ignored (Archon does
    // not track named constraints), but the syntax must be consumed.
    if let Tok::Ident(kw) = ts.peek() {
        if eq_ignore_case(&kw, "constraint") {
            ts.next();
            expect_ident(ts, "constraint name")?;
        }
    }
    match ts.peek() {
        Tok::Ident(kw) if eq_ignore_case(&kw, "primary") => {
            ts.next();
            expect_kw(ts, "key")?;
            let cols = parse_paren_column_list(ts)?;
            Ok(TableConstraint::PrimaryKey(cols))
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "unique") => {
            ts.next();
            let cols = parse_paren_column_list(ts)?;
            Ok(TableConstraint::Unique(cols))
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "foreign") => {
            ts.next();
            expect_kw(ts, "key")?;
            let columns = parse_paren_column_list(ts)?;
            expect_kw(ts, "references")?;
            let ref_table = expect_ident(ts, "referenced table")?;
            let ref_columns = parse_paren_column_list(ts)?;
            Ok(TableConstraint::ForeignKey {
                columns,
                ref_table,
                ref_columns,
            })
        }
        Tok::Ident(kw) if eq_ignore_case(&kw, "check") => {
            ts.next();
            expect_tok(ts, Tok::LParen, "'(' after CHECK")?;
            let expr = super::expr::parse_expr(ts)?;
            expect_tok(ts, Tok::RParen, "')' after CHECK expression")?;
            Ok(TableConstraint::Check(expr))
        }
        _ => Err(ParseError(
            "expected PRIMARY KEY, UNIQUE, FOREIGN KEY, or CHECK".to_string(),
        )),
    }
}

/// Parses a parenthesized, comma-separated list of column names.
fn parse_paren_column_list(ts: &mut TokenStream) -> Result<Vec<String>, ParseError> {
    expect_tok(ts, Tok::LParen, "'('")?;
    let mut cols = Vec::new();
    loop {
        cols.push(expect_ident(ts, "column name")?);
        match ts.next() {
            Tok::Comma => continue,
            Tok::RParen => break,
            _ => return Err(ParseError("expected ',' or ')'".to_string())),
        }
    }
    Ok(cols)
}

pub(super) fn parse_create_table(ts: &mut TokenStream) -> Result<CreateTableStatement, ParseError> {
    expect_kw(ts, "table")?;
    let table = expect_ident(ts, "table name")?;
    expect_tok(ts, Tok::LParen, "'('")?;
    let mut columns = Vec::new();
    let mut constraints = Vec::new();
    loop {
        if is_table_constraint(ts) {
            constraints.push(parse_table_constraint(ts)?);
        } else {
            let name = expect_ident(ts, "column name")?;
            let ctype = parse_column_type(ts)?;
            let col_constraints = parse_column_constraints(ts)?;
            columns.push(ColumnDef {
                name: name.clone(),
                ctype,
                constraints: col_constraints,
            });
            // Column-level `REFERENCES` foreign key: `col type REFERENCES other(cols)`.
            if let Tok::Ident(kw) = ts.peek() {
                if eq_ignore_case(&kw, "references") {
                    ts.next();
                    let ref_table = expect_ident(ts, "referenced table")?;
                    let ref_columns = parse_paren_column_list(ts)?;
                    constraints.push(TableConstraint::ForeignKey {
                        columns: alloc::vec![name],
                        ref_table,
                        ref_columns,
                    });
                }
            }
        }
        // Items are normally comma-separated, but PostgreSQL also allows a
        // constraint keyword (e.g. a trailing `CHECK`) to follow a column
        // definition without an intervening comma, so we re-loop on a
        // constraint-start token as well.
        match ts.peek() {
            Tok::Comma => {
                ts.next();
            }
            Tok::RParen => {
                ts.next();
                break;
            }
            _ if is_table_constraint(ts) => {}
            _ => return Err(ParseError("expected ',' or ')'".to_string())),
        }
    }
    Ok(CreateTableStatement {
        table,
        columns,
        constraints,
    })
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
            AlterTableOp::AddColumn(ColumnDef {
                name,
                ctype,
                constraints: Vec::new(),
            })
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
