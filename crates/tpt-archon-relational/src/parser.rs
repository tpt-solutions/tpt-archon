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

/// A value literal in an `INSERT`/`UPDATE` value list.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// An integer literal.
    Int(i64),
    /// A single-quoted text literal.
    Text(String),
    /// A bracketed `f32[]` vector literal, e.g. `[0.1, 0.9]`.
    Vector(Vec<f32>),
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
    /// Optional `ORDER BY cosine(emb, ?) LIMIT k` for vector top-k.
    pub order_by_cosine: Option<OrderByCosine>,
    /// Optional `LIMIT`.
    pub limit: Option<u64>,
}

/// An `ORDER BY cosine(<col>, <param>) LIMIT k` clause (RAG/embedding top-k).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderByCosine {
    /// The embedding column to compare against.
    pub column: String,
    /// The `?` placeholder index (1-based) supplying the query vector.
    pub param: usize,
    /// The number of nearest neighbours to return.
    pub k: u64,
}

/// A column assignment in `UPDATE`/`INSERT`.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    /// The column to assign.
    pub column: String,
    /// The literal value.
    pub value: Literal,
}

/// A parsed `INSERT INTO t (c, ...) VALUES (v, ...)` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStatement {
    /// Target table.
    pub table: String,
    /// Column names in assignment order.
    pub columns: Vec<String>,
    /// Literal values, positionally aligned with `columns`.
    pub values: Vec<Literal>,
}

/// A parsed `UPDATE t SET c = v, ... [WHERE pred]` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatement {
    /// Target table.
    pub table: String,
    /// Assignments.
    pub assignments: Vec<Assignment>,
    /// Optional `WHERE` predicate.
    pub filter: Option<Predicate>,
}

/// A parsed `DELETE FROM t [WHERE pred]` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteStatement {
    /// Target table.
    pub table: String,
    /// Optional `WHERE` predicate.
    pub filter: Option<Predicate>,
}

/// A fully parsed statement: any of the supported DML/DQL forms.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// A `SELECT` query.
    Select(SelectStatement),
    /// An `INSERT` statement.
    Insert(InsertStatement),
    /// An `UPDATE` statement.
    Update(UpdateStatement),
    /// A `DELETE` statement.
    Delete(DeleteStatement),
}

/// A parse error with a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

#[derive(Debug, Clone, PartialEq)]
enum Tok<'a> {
    Ident(&'a str),
    Int(i64),
    Float(f32),
    Text(&'a str),
    Star,
    Comma,
    Op(CmpOp),
    Param,
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
            b'?' => {
                self.pos += 1;
                Ok(Tok::Param)
            }
            b'\'' => {
                // Single-quoted text literal; consume until the closing quote.
                self.pos += 1;
                let start = self.pos;
                while self.pos < b.len() && b[self.pos] != b'\'' {
                    self.pos += 1;
                }
                if self.pos >= b.len() {
                    return Err(ParseError("unterminated text literal".to_string()));
                }
                let text = &self.s[start..self.pos];
                self.pos += 1; // skip closing quote
                Ok(Tok::Text(text))
            }
            c if c.is_ascii_digit() || c == b'-' || c == b'.' => {
                let start = self.pos;
                if c == b'-' {
                    self.pos += 1;
                }
                while self.pos < b.len() && b[self.pos].is_ascii_digit() {
                    self.pos += 1;
                }
                let mut is_float = false;
                if self.pos < b.len() && b[self.pos] == b'.' {
                    is_float = true;
                    self.pos += 1;
                    while self.pos < b.len() && b[self.pos].is_ascii_digit() {
                        self.pos += 1;
                    }
                }
                let text = &self.s[start..self.pos];
                if is_float {
                    text.parse::<f32>()
                        .map(Tok::Float)
                        .map_err(|_| ParseError("invalid float".to_string()))
                } else {
                    text.parse::<i64>()
                        .map(Tok::Int)
                        .map_err(|_| ParseError("invalid integer".to_string()))
                }
            }
            c if c.is_ascii_alphabetic()
                || c == b'_'
                || c == b'('
                || c == b')'
                || c == b'['
                || c == b']' =>
            {
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
    let mut order_by_cosine = None;

    // Optional WHERE / ORDER BY / LIMIT (each may appear at most once, in any
    // of the supported orders; we loop so `ORDER BY` may precede or follow
    // `LIMIT`, and `WHERE` may precede either).
    let mut t = lx.next_tok()?;
    loop {
        match t {
            Tok::Ident(kw) if eq_ignore_case(kw, "where") => {
                if filter.is_some() {
                    return Err(ParseError("duplicate WHERE".to_string()));
                }
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
            Tok::Ident(kw) if eq_ignore_case(kw, "order") => {
                if order_by_cosine.is_some() {
                    return Err(ParseError("duplicate ORDER BY".to_string()));
                }
                // Expect: BY cosine ( col , ? ) LIMIT k
                match lx.next_tok()? {
                    Tok::Ident(kw) if eq_ignore_case(kw, "by") => {}
                    _ => return Err(ParseError("expected BY after ORDER".to_string())),
                }
                match lx.next_tok()? {
                    Tok::Ident(kw) if eq_ignore_case(kw, "cosine") => {}
                    _ => {
                        return Err(ParseError(
                            "only ORDER BY cosine(...) is supported".to_string(),
                        ))
                    }
                }
                match lx.next_tok()? {
                    Tok::Ident(_) => {}
                    _ => return Err(ParseError("expected '(' after cosine".to_string())),
                }
                let column = match lx.next_tok()? {
                    Tok::Ident(name) => name.to_string(),
                    _ => return Err(ParseError("expected embedding column".to_string())),
                };
                match lx.next_tok()? {
                    Tok::Comma => {}
                    _ => return Err(ParseError("expected ',' in cosine(...)".to_string())),
                }
                let param = match lx.next_tok()? {
                    Tok::Param => 1,
                    _ => return Err(ParseError("expected '?' query vector".to_string())),
                };
                match lx.next_tok()? {
                    Tok::Ident(_) => {}
                    _ => return Err(ParseError("expected ')' after cosine(...)".to_string())),
                }
                // LIMIT k is required for a top-k.
                match lx.next_tok()? {
                    Tok::Ident(kw) if eq_ignore_case(kw, "limit") => {}
                    _ => return Err(ParseError("ORDER BY cosine requires LIMIT k".to_string())),
                }
                let k = match lx.next_tok()? {
                    Tok::Int(v) if v > 0 => v as u64,
                    _ => return Err(ParseError("expected positive LIMIT k".to_string())),
                };
                order_by_cosine = Some(OrderByCosine { column, param, k });
                t = lx.next_tok()?;
            }
            Tok::Ident(kw) if eq_ignore_case(kw, "limit") => {
                if limit.is_some() {
                    return Err(ParseError("duplicate LIMIT".to_string()));
                }
                match lx.next_tok()? {
                    Tok::Int(v) if v >= 0 => limit = Some(v as u64),
                    _ => return Err(ParseError("expected non-negative LIMIT".to_string())),
                }
                t = lx.next_tok()?;
            }
            _ => break,
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
        order_by_cosine,
        limit,
    })
}

/// Parses any supported statement (`SELECT` / `INSERT` / `UPDATE` / `DELETE`).
pub fn parse_statement(input: &str) -> Result<Statement, ParseError> {
    let mut lx = Lexer::new(input);
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "select") => {
            parse_select(input).map(Statement::Select)
        }
        Tok::Ident(kw) if eq_ignore_case(kw, "insert") => parse_insert(lx).map(Statement::Insert),
        Tok::Ident(kw) if eq_ignore_case(kw, "update") => parse_update(lx).map(Statement::Update),
        Tok::Ident(kw) if eq_ignore_case(kw, "delete") => parse_delete(lx).map(Statement::Delete),
        _ => Err(ParseError(
            "expected SELECT, INSERT, UPDATE, or DELETE".to_string(),
        )),
    }
}

/// Parses a bracketed `f32[]` vector literal `[a, b, c]` from `lx`, where the
/// next token is expected to be `[`.
fn parse_vector_literal(lx: &mut Lexer) -> Result<Literal, ParseError> {
    // The lexer turns '[' into an Ident("[". Re-read the raw bytes to scan the
    // numeric list between brackets.
    let b = lx.bytes();
    if lx.pos >= b.len() || b[lx.pos] != b'[' {
        return Err(ParseError("expected '['".to_string()));
    }
    lx.pos += 1; // consume '['
    let mut vals = Vec::new();
    // Scan comma-separated floats until ']'.
    loop {
        while lx.pos < b.len() && b[lx.pos].is_ascii_whitespace() {
            lx.pos += 1;
        }
        if lx.pos >= b.len() {
            return Err(ParseError("unterminated vector literal".to_string()));
        }
        if b[lx.pos] == b']' {
            lx.pos += 1;
            break;
        }
        let start = lx.pos;
        while lx.pos < b.len() && b[lx.pos] != b',' && b[lx.pos] != b']' {
            lx.pos += 1;
        }
        let text = &lx.s[start..lx.pos];
        let f: f32 = text
            .trim()
            .parse()
            .map_err(|_| ParseError("invalid vector element".to_string()))?;
        vals.push(f);
        while lx.pos < b.len() && b[lx.pos].is_ascii_whitespace() {
            lx.pos += 1;
        }
        if lx.pos < b.len() && b[lx.pos] == b',' {
            lx.pos += 1;
        }
    }
    Ok(Literal::Vector(vals))
}

fn expect_ident(lx: &mut Lexer, what: &str) -> Result<String, ParseError> {
    match lx.next_tok()? {
        Tok::Ident(name) => Ok(name.to_string()),
        _ => Err(ParseError(alloc::format!("expected {what}"))),
    }
}

fn parse_insert(mut lx: Lexer) -> Result<InsertStatement, ParseError> {
    // INSERT INTO <table> (c1, c2, ...) VALUES (v1, v2, ...)
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "into") => {}
        _ => return Err(ParseError("expected INTO".to_string())),
    }
    let table = expect_ident(&mut lx, "table name")?;

    // Optional column list: (c1, c2, ...)
    let mut columns = Vec::new();
    let mut t = lx.next_tok()?;
    if let Tok::Ident(p) = &t {
        if *p == "(" {
            loop {
                let col = expect_ident(&mut lx, "column name")?;
                columns.push(col);
                match lx.next_tok()? {
                    Tok::Ident(",") => continue,
                    Tok::Ident(")") => break,
                    _ => return Err(ParseError("expected ',' or ')'".to_string())),
                }
            }
            t = lx.next_tok()?;
        }
    }

    // VALUES
    match t {
        Tok::Ident(kw) if eq_ignore_case(kw, "values") => {}
        _ => return Err(ParseError("expected VALUES".to_string())),
    }
    // (v1, v2, ...)
    match lx.next_tok()? {
        Tok::Ident("(") => {}
        _ => return Err(ParseError("expected '(' before values".to_string())),
    }
    let mut values = Vec::new();
    loop {
        let value = match lx.next_tok()? {
            Tok::Int(v) => Literal::Int(v),
            Tok::Text(s) => Literal::Text(s.to_string()),
            Tok::Ident("[") => parse_vector_literal(&mut lx)?,
            _ => return Err(ParseError("expected a value".to_string())),
        };
        values.push(value);
        match lx.next_tok()? {
            Tok::Ident(",") => continue,
            Tok::Ident(")") => break,
            _ => return Err(ParseError("expected ',' or ')'".to_string())),
        }
    }
    if lx.next_tok()? != Tok::Eof {
        return Err(ParseError("trailing tokens after statement".to_string()));
    }
    Ok(InsertStatement {
        table,
        columns,
        values,
    })
}

fn parse_update(mut lx: Lexer) -> Result<UpdateStatement, ParseError> {
    let table = expect_ident(&mut lx, "table name")?;
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "set") => {}
        _ => return Err(ParseError("expected SET".to_string())),
    }
    let mut assignments = Vec::new();
    loop {
        let column = expect_ident(&mut lx, "column name")?;
        match lx.next_tok()? {
            Tok::Op(CmpOp::Eq) => {}
            _ => return Err(ParseError("expected '=' in assignment".to_string())),
        }
        let value = match lx.next_tok()? {
            Tok::Int(v) => Literal::Int(v),
            Tok::Text(s) => Literal::Text(s.to_string()),
            Tok::Ident("[") => parse_vector_literal(&mut lx)?,
            _ => return Err(ParseError("expected a value in assignment".to_string())),
        };
        assignments.push(Assignment { column, value });
        match lx.next_tok()? {
            Tok::Comma => continue,
            t => {
                // Hand the token back conceptually by checking WHERE/EOF.
                if let Tok::Ident(kw) = t {
                    if eq_ignore_case(kw, "where") {
                        let column = expect_ident(&mut lx, "column in WHERE")?;
                        let op = match lx.next_tok()? {
                            Tok::Op(op) => op,
                            _ => return Err(ParseError("expected operator in WHERE".to_string())),
                        };
                        let value = match lx.next_tok()? {
                            Tok::Int(v) => v,
                            _ => return Err(ParseError("expected integer in WHERE".to_string())),
                        };
                        let filter = Some(Predicate { column, op, value });
                        if lx.next_tok()? != Tok::Eof {
                            return Err(ParseError("trailing tokens".to_string()));
                        }
                        return Ok(UpdateStatement {
                            table,
                            assignments,
                            filter,
                        });
                    }
                }
                if t != Tok::Eof {
                    return Err(ParseError("trailing tokens".to_string()));
                }
                return Ok(UpdateStatement {
                    table,
                    assignments,
                    filter: None,
                });
            }
        }
    }
}

fn parse_delete(mut lx: Lexer) -> Result<DeleteStatement, ParseError> {
    // DELETE FROM <table> [WHERE col op int]
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "from") => {}
        _ => return Err(ParseError("expected FROM".to_string())),
    }
    let table = expect_ident(&mut lx, "table name")?;
    let mut filter = None;
    let t = lx.next_tok()?;
    if let Tok::Ident(kw) = t {
        if eq_ignore_case(kw, "where") {
            let column = expect_ident(&mut lx, "column in WHERE")?;
            let op = match lx.next_tok()? {
                Tok::Op(op) => op,
                _ => return Err(ParseError("expected operator in WHERE".to_string())),
            };
            let value = match lx.next_tok()? {
                Tok::Int(v) => v,
                _ => return Err(ParseError("expected integer in WHERE".to_string())),
            };
            filter = Some(Predicate { column, op, value });
        }
    }
    if lx.next_tok()? != Tok::Eof {
        return Err(ParseError("trailing tokens after statement".to_string()));
    }
    Ok(DeleteStatement { table, filter })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_insert_tokens() {
        let mut lx = Lexer::new("INSERT INTO users (id, name, age) VALUES (1, 'alice', 30)");
        let mut toks = Vec::new();
        loop {
            match lx.next_tok() {
                Ok(Tok::Eof) => {
                    toks.push("EOF".to_string());
                    break;
                }
                Ok(t) => toks.push(format!("{t:?}")),
                Err(e) => {
                    toks.push(format!("ERR {e:?}"));
                    break;
                }
            }
        }
        panic!("TOKENS: {}", toks.join(" | "));
    }

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
