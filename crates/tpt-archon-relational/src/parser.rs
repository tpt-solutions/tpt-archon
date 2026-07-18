//! A hand-written, allocation-light SQL parser (PostgreSQL-leaning dialect).
//!
//! This is a deliberately small but real recursive-descent parser covering the
//! subset the executor supports today:
//!
//! ```sql
//! SELECT <col, ...> FROM <table> [WHERE <col> <op> <int>] [LIMIT <n>]
//! ```
//!
//! It uses a zero-copy tokenizer that borrows directly from the input string.
//! PostgreSQL compatibility is the target dialect (spec Risk 2: PostgreSQL
//! first, SQLite later); the grammar grows from here.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A comparison operator in a `WHERE` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>` or `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A `WHERE column <op> value` predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Predicate {
    /// The column being compared.
    pub column: String,
    /// The comparison operator.
    pub op: CmpOp,
    /// The right-hand integer literal.
    pub value: i64,
}

/// A parsed `SELECT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    /// Projected columns; a single `*` becomes an empty vec meaning "all".
    pub columns: Vec<String>,
    /// Whether the projection was `*`.
    pub star: bool,
    /// The source table name.
    pub table: String,
    /// Optional `WHERE` predicate.
    pub filter: Option<Predicate>,
    /// Optional `LIMIT`.
    pub limit: Option<u64>,
}

/// A parse error with a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok<'a> {
    Ident(&'a str),
    Int(i64),
    Star,
    Comma,
    Op(CmpOp),
    Eof,
}

struct Lexer<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, pos: 0 }
    }

    fn bytes(&self) -> &'a [u8] {
        self.s.as_bytes()
    }

    fn next_tok(&mut self) -> Result<Tok<'a>, ParseError> {
        let b = self.bytes();
        while self.pos < b.len() && b[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        if self.pos >= b.len() {
            return Ok(Tok::Eof);
        }
        let c = b[self.pos];
        match c {
            b'*' => {
                self.pos += 1;
                Ok(Tok::Star)
            }
            b',' => {
                self.pos += 1;
                Ok(Tok::Comma)
            }
            b'=' => {
                self.pos += 1;
                Ok(Tok::Op(CmpOp::Eq))
            }
            b'<' => {
                self.pos += 1;
                if self.pos < b.len() && b[self.pos] == b'=' {
                    self.pos += 1;
                    Ok(Tok::Op(CmpOp::Le))
                } else if self.pos < b.len() && b[self.pos] == b'>' {
                    self.pos += 1;
                    Ok(Tok::Op(CmpOp::Ne))
                } else {
                    Ok(Tok::Op(CmpOp::Lt))
                }
            }
            b'>' => {
                self.pos += 1;
                if self.pos < b.len() && b[self.pos] == b'=' {
                    self.pos += 1;
                    Ok(Tok::Op(CmpOp::Ge))
                } else {
                    Ok(Tok::Op(CmpOp::Gt))
                }
            }
            b'!' => {
                self.pos += 1;
                if self.pos < b.len() && b[self.pos] == b'=' {
                    self.pos += 1;
                    Ok(Tok::Op(CmpOp::Ne))
                } else {
                    Err(ParseError("unexpected '!'".to_string()))
                }
            }
            c if c.is_ascii_digit() || c == b'-' => {
                let start = self.pos;
                if c == b'-' {
                    self.pos += 1;
                }
                while self.pos < b.len() && b[self.pos].is_ascii_digit() {
                    self.pos += 1;
                }
                let text = &self.s[start..self.pos];
                text.parse::<i64>()
                    .map(Tok::Int)
                    .map_err(|_| ParseError("invalid integer".to_string()))
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = self.pos;
                while self.pos < b.len()
                    && (b[self.pos].is_ascii_alphanumeric() || b[self.pos] == b'_')
                {
                    self.pos += 1;
                }
                Ok(Tok::Ident(&self.s[start..self.pos]))
            }
            other => Err(ParseError(alloc::format!(
                "unexpected character '{}'",
                other as char
            ))),
        }
    }
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

/// Parses a single `SELECT` statement.
pub fn parse_select(input: &str) -> Result<SelectStatement, ParseError> {
    let mut lx = Lexer::new(input);
    let first = lx.next_tok()?;
    match first {
        Tok::Ident(kw) if eq_ignore_case(kw, "select") => {}
        _ => return Err(ParseError("expected SELECT".to_string())),
    }

    // Columns.
    let mut columns = Vec::new();
    let mut star = false;
    let mut t = lx.next_tok()?;
    if t == Tok::Star {
        star = true;
        t = lx.next_tok()?;
    } else {
        loop {
            match t {
                Tok::Ident(name) => columns.push(name.to_string()),
                _ => return Err(ParseError("expected column name".to_string())),
            }
            t = lx.next_tok()?;
            if t == Tok::Comma {
                t = lx.next_tok()?;
                continue;
            }
            break;
        }
    }

    // FROM.
    match t {
        Tok::Ident(kw) if eq_ignore_case(kw, "from") => {}
        _ => return Err(ParseError("expected FROM".to_string())),
    }
    let table = match lx.next_tok()? {
        Tok::Ident(name) => name.to_string(),
        _ => return Err(ParseError("expected table name".to_string())),
    };

    let mut filter = None;
    let mut limit = None;

    // Optional WHERE / LIMIT.
    let mut t = lx.next_tok()?;
    if let Tok::Ident(kw) = t {
        if eq_ignore_case(kw, "where") {
            let column = match lx.next_tok()? {
                Tok::Ident(name) => name.to_string(),
                _ => return Err(ParseError("expected column in WHERE".to_string())),
            };
            let op = match lx.next_tok()? {
                Tok::Op(op) => op,
                _ => return Err(ParseError("expected operator in WHERE".to_string())),
            };
            let value = match lx.next_tok()? {
                Tok::Int(v) => v,
                _ => return Err(ParseError("expected integer in WHERE".to_string())),
            };
            filter = Some(Predicate { column, op, value });
            t = lx.next_tok()?;
        }
    }

    if let Tok::Ident(kw) = t {
        if eq_ignore_case(kw, "limit") {
            match lx.next_tok()? {
                Tok::Int(v) if v >= 0 => limit = Some(v as u64),
                _ => return Err(ParseError("expected non-negative LIMIT".to_string())),
            }
            t = lx.next_tok()?;
        }
    }

    if t != Tok::Eof {
        return Err(ParseError("trailing tokens after statement".to_string()));
    }

    Ok(SelectStatement {
        columns,
        star,
        table,
        filter,
        limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_star_select() {
        let s = parse_select("SELECT * FROM users").unwrap();
        assert!(s.star);
        assert_eq!(s.table, "users");
        assert!(s.filter.is_none());
        assert!(s.limit.is_none());
    }

    #[test]
    fn parses_columns_where_limit() {
        let s = parse_select("SELECT id, name FROM t WHERE age >= 18 LIMIT 5").unwrap();
        assert!(!s.star);
        assert_eq!(s.columns, alloc::vec!["id".to_string(), "name".to_string()]);
        assert_eq!(s.table, "t");
        assert_eq!(
            s.filter,
            Some(Predicate {
                column: "age".to_string(),
                op: CmpOp::Ge,
                value: 18
            })
        );
        assert_eq!(s.limit, Some(5));
    }

    #[test]
    fn all_operators() {
        for (src, op) in [
            ("=", CmpOp::Eq),
            ("<>", CmpOp::Ne),
            ("!=", CmpOp::Ne),
            ("<", CmpOp::Lt),
            ("<=", CmpOp::Le),
            (">", CmpOp::Gt),
            (">=", CmpOp::Ge),
        ] {
            let sql = alloc::format!("SELECT * FROM t WHERE x {src} 1");
            let s = parse_select(&sql).unwrap();
            assert_eq!(s.filter.unwrap().op, op, "op {src}");
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_select("UPDATE t SET x=1").is_err());
        assert!(parse_select("SELECT FROM t").is_err());
        assert!(parse_select("SELECT * FROM t EXTRA").is_err());
    }
}
