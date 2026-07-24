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
//! `BETWEEN`, and comparisons against integer, text, or `NULL` literals.
//!
//! It uses a zero-copy tokenizer that borrows directly from the input string.
//! PostgreSQL compatibility is the target dialect (spec Risk 2: PostgreSQL
//! first, SQLite later); the grammar grows from here.

use alloc::boxed::Box;
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

/// A value literal in an `INSERT`/`UPDATE` value list or expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// An integer literal.
    Int(i64),
    /// A single-quoted text literal.
    Text(String),
    /// A bracketed `f32[]` vector literal, e.g. `[0.1, 0.9]`.
    Vector(Vec<f32>),
    /// SQL `NULL`.
    Null,
}

/// A boolean expression tree used in `WHERE` clauses, `HAVING`, etc.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A simple comparison: `column <op> value`.
    Cmp {
        /// The column being compared.
        column: String,
        /// The comparison operator.
        op: CmpOp,
        /// The right-hand value.
        value: Literal,
    },
    /// A column-to-column comparison: `left_col <op> right_col`.
    CmpColumn {
        /// The left-hand column.
        left: String,
        /// The comparison operator.
        op: CmpOp,
        /// The right-hand column.
        right: String,
    },
    /// `column IS [NOT] NULL`.
    IsNull {
        /// The column.
        column: String,
        /// `true` for `IS NOT NULL` / `!= NULL`.
        negated: bool,
    },
    /// `column LIKE pattern` (simple `%` and `_` wildcards).
    Like {
        /// The column to match.
        column: String,
        /// The pattern string.
        pattern: String,
    },
    /// `column IN (v1, v2, ...)`.
    InInt {
        /// The column.
        column: String,
        /// The list of integer values.
        values: Vec<i64>,
    },
    /// `column BETWEEN low AND high` (inclusive).
    BetweenInt {
        /// The column.
        column: String,
        /// The lower bound (inclusive).
        low: i64,
        /// The upper bound (inclusive).
        high: i64,
    },
    /// `expr AND expr`.
    And(Box<Expr>, Box<Expr>),
    /// `expr OR expr`.
    Or(Box<Expr>, Box<Expr>),
    /// `NOT expr`.
    Not(Box<Expr>),
    /// `EXISTS (SELECT ...)`. `NOT EXISTS` is `Expr::Not` wrapping this.
    Exists {
        /// The subquery whose row count is tested.
        query: Box<SelectStatement>,
    },
    /// `column IN (SELECT ...)`. `NOT IN (SELECT ...)` is `Expr::Not` wrapping this.
    InSubquery {
        /// The column being tested for membership.
        column: String,
        /// The subquery producing the candidate set; must yield exactly one column.
        query: Box<SelectStatement>,
    },
    /// `column <op> (SELECT ...)` — a scalar subquery comparison. The subquery
    /// must yield exactly one row and one column at evaluation time, or
    /// evaluation fails (mirrors PostgreSQL's "more than one row returned by
    /// a subquery used as an expression").
    ScalarCmp {
        /// The column being compared.
        column: String,
        /// The comparison operator.
        op: CmpOp,
        /// The subquery producing the right-hand scalar value.
        query: Box<SelectStatement>,
    },
    /// An aggregate function used in `HAVING` or `SELECT` expressions:
    /// `COUNT(*)`, `SUM(col)`, `AVG(col)`, `MIN(col)`, `MAX(col)`.
    Agg {
        /// The aggregate function.
        func: AggregateFunc,
        /// The column argument (`*` for `COUNT(*)`).
        column: String,
    },
}

/// A column reference with optional sort direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    /// The column name (or aggregate alias).
    pub column: String,
    /// Sort direction; `true` = descending.
    pub descending: bool,
}

/// An aggregate function applied in `SELECT` or `HAVING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    /// `COUNT(*)` or `COUNT(col)`.
    Count,
    /// `SUM(col)`.
    Sum,
    /// `AVG(col)`.
    Avg,
    /// `MIN(col)`.
    Min,
    /// `MAX(col)`.
    Max,
}

/// A column definition in `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// Column name.
    pub name: String,
    /// Column type.
    pub ctype: ColumnType,
}

/// A column type in SQL DDL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// `INT` — 64-bit integer.
    Int,
    /// `TEXT` — UTF-8 text.
    Text,
    /// `VECTOR` — fixed-width `f32` embedding.
    Vector,
}

/// A join type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    /// `JOIN` or `INNER JOIN`.
    Inner,
}

/// A reference to a table, view, CTE, or derived (subquery) source in a
/// `FROM`/`JOIN` clause.
#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    /// A plain table/view/CTE name, optionally with an `AS alias`.
    Named {
        /// The underlying table name.
        name: String,
        /// Optional `AS alias` (used for table-qualified column resolution in
        /// correlated subqueries).
        alias: Option<String>,
    },
    /// A derived table: `(SELECT ...) AS alias`.
    Subquery {
        /// The subquery producing the derived table's rows.
        query: Box<SelectStatement>,
        /// The alias the derived table is referenced by.
        alias: String,
    },
}

impl TableRef {
    /// The effective name this reference is known by: the alias if present,
    /// otherwise the underlying table name.  Used for display/estimation and
    /// for qualifying column names in correlated-subquery scope resolution.
    pub fn name(&self) -> &str {
        match self {
            TableRef::Named { alias, name, .. } => alias.as_deref().unwrap_or(name),
            TableRef::Subquery { alias, .. } => alias,
        }
    }

    /// The underlying table name (without any alias).
    pub fn table_name(&self) -> &str {
        match self {
            TableRef::Named { name, .. } => name,
            TableRef::Subquery { alias, .. } => alias,
        }
    }
}

/// A join clause: `JOIN <table> ON <left_col> = <right_col>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// Join type.
    pub jtype: JoinType,
    /// The table to join with.
    pub table: TableRef,
    /// The column from the left table.
    pub left_col: String,
    /// The column from the right table.
    pub right_col: String,
}

/// A parsed `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    /// Projected columns; a single `*` becomes an empty vec meaning "all".
    pub columns: Vec<String>,
    /// Whether the projection was `*`.
    pub star: bool,
    /// The source table reference (table, view, CTE, or derived subquery).
    pub table: TableRef,
    /// Optional `WHERE` expression tree.
    pub filter: Option<Expr>,
    /// Optional `JOIN` clauses.
    pub joins: Vec<Join>,
    /// Optional `GROUP BY` columns.
    pub group_by: Vec<String>,
    /// Optional aggregate projections: `alias -> (func, column)`.
    pub aggregates: Vec<(String, AggregateFunc, String)>,
    /// Optional `ORDER BY` columns.
    pub order_by: Vec<OrderBy>,
    /// Optional `ORDER BY cosine(emb, ?) LIMIT k` for vector top-k (legacy).
    pub order_by_cosine: Option<OrderByCosine>,
    /// Optional `LIMIT`.
    pub limit: Option<u64>,
    /// Optional `HAVING` expression (applied after GROUP BY + aggregates).
    pub having: Option<Expr>,
    /// Common Table Expressions defined before this SELECT.
    pub with_ctes: Vec<CTE>,
}

/// A Common Table Expression: `name AS (SELECT ...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CTE {
    /// The CTE name, usable as a table reference in the main query.
    pub name: String,
    /// The CTE's defining query.
    pub query: SelectStatement,
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

/// A parsed `UPDATE t SET c = v, ... [WHERE expr]` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatement {
    /// Target table.
    pub table: String,
    /// Assignments.
    pub assignments: Vec<Assignment>,
    /// Optional `WHERE` expression tree.
    pub filter: Option<Expr>,
}

/// A parsed `DELETE FROM t [WHERE expr]` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    /// Target table.
    pub table: String,
    /// Optional `WHERE` expression tree.
    pub filter: Option<Expr>,
}

/// A parsed `CREATE TABLE t (col type, ...)` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStatement {
    /// The table name.
    pub table: String,
    /// Column definitions.
    pub columns: Vec<ColumnDef>,
}

/// A parsed `CREATE VIEW name AS <select>` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateViewStatement {
    /// The view name.
    pub name: String,
    /// The view's defining query.
    pub query: SelectStatement,
}

/// A parsed `ALTER TABLE` operation.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableOp {
    /// `ALTER TABLE t ADD COLUMN name type`
    AddColumn(ColumnDef),
    /// `ALTER TABLE t DROP COLUMN name`
    DropColumn(String),
    /// `ALTER TABLE t RENAME COLUMN old TO new`
    RenameColumn { old_name: String, new_name: String },
}

/// A parsed `ALTER TABLE t <op>` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTableStatement {
    /// The table to alter.
    pub table: String,
    /// The operation to perform.
    pub op: AlterTableOp,
}

/// A fully parsed statement: any of the supported DML/DQL/DDL forms.
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
    /// A `CREATE TABLE` statement.
    CreateTable(CreateTableStatement),
    /// A `CREATE VIEW` statement.
    CreateView(CreateViewStatement),
    /// A `DROP VIEW name` statement.
    DropView(String),
    /// An `ALTER TABLE` statement.
    AlterTable(AlterTableStatement),
    /// A `BEGIN` transaction statement.
    Begin,
    /// A `COMMIT` transaction statement.
    Commit,
    /// A `ROLLBACK` transaction statement.
    Rollback,
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
    Dot,
    Op(CmpOp),
    Param,
    LParen,
    RParen,
    LBracket,
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
            b'(' => {
                self.pos += 1;
                Ok(Tok::LParen)
            }
            b')' => {
                self.pos += 1;
                Ok(Tok::RParen)
            }
            b'.' => {
                self.pos += 1;
                Ok(Tok::Dot)
            }
            b'[' => {
                self.pos += 1;
                Ok(Tok::LBracket)
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
                self.pos += 1;
                let start = self.pos;
                while self.pos < b.len() && b[self.pos] != b'\'' {
                    self.pos += 1;
                }
                if self.pos >= b.len() {
                    return Err(ParseError("unterminated text literal".to_string()));
                }
                let text = &self.s[start..self.pos];
                self.pos += 1;
                Ok(Tok::Text(text))
            }
            c if c.is_ascii_digit() || c == b'-' => {
                let start = self.pos;
                if c == b'-' {
                    self.pos += 1;
                }
                while self.pos < b.len() && b[self.pos].is_ascii_digit() {
                    self.pos += 1;
                }
                let mut is_float = false;
                if self.pos < b.len() && b[self.pos] == b'.' {
                    // Only treat as float if followed by digits AND the next
                    // char is NOT alphanumeric (to avoid eating `t1.id`).
                    let dot_pos = self.pos;
                    self.pos += 1;
                    if self.pos < b.len() && b[self.pos].is_ascii_digit() {
                        while self.pos < b.len() && b[self.pos].is_ascii_digit() {
                            self.pos += 1;
                        }
                        // Check that this isn't `t1.id` — if the digit run
                        // is followed by an alpha char, it's a qualified name.
                        if self.pos < b.len() && b[self.pos].is_ascii_alphabetic() {
                            // Not a float — revert and return just the integer.
                            self.pos = dot_pos;
                        } else {
                            is_float = true;
                        }
                    } else {
                        // Dot followed by non-digit: revert.
                        self.pos = dot_pos;
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

fn expect_ident(lx: &mut Lexer, what: &str) -> Result<String, ParseError> {
    let tok = lx.next_tok()?;
    match tok {
        Tok::Ident(name) => {
            // Check for qualified name: ident.ident (e.g. t1.id).
            let next = lx.next_tok()?;
            if let Tok::Dot = next {
                let field = expect_ident(lx, "field name after '.'")?;
                Ok(alloc::format!("{name}.{field}"))
            } else {
                push_back(lx, next);
                Ok(name.to_string())
            }
        }
        _ => Err(ParseError(alloc::format!("expected {what}"))),
    }
}

fn expect_kw(lx: &mut Lexer, expected: &str) -> Result<(), ParseError> {
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, expected) => Ok(()),
        _ => Err(ParseError(alloc::format!("expected {expected}"))),
    }
}

fn is_kw(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "select"
            | "insert"
            | "update"
            | "delete"
            | "from"
            | "where"
            | "set"
            | "order"
            | "limit"
            | "group"
            | "having"
            | "join"
            | "on"
            | "create"
            | "view"
            | "drop"
            | "begin"
            | "commit"
            | "rollback"
            | "not"
            | "exists"
    )
}

// ---------------------------------------------------------------------------
// Expression parsing (WHERE / HAVING)
// ---------------------------------------------------------------------------

fn parse_expr(lx: &mut Lexer) -> Result<Expr, ParseError> {
    parse_or(lx)
}

fn parse_or(lx: &mut Lexer) -> Result<Expr, ParseError> {
    let mut left = parse_and(lx)?;
    loop {
        match lx.next_tok()? {
            Tok::Ident(kw) if eq_ignore_case(kw, "or") => {
                let right = parse_and(lx)?;
                left = Expr::Or(Box::new(left), Box::new(right));
            }
            tok => {
                // Push the token back conceptually.
                push_back(lx, tok);
                break;
            }
        }
    }
    Ok(left)
}

fn push_back(lx: &mut Lexer, tok: Tok) {
    match tok {
        Tok::Ident(s) => lx.pos -= s.len(),
        Tok::Int(v) => {
            let len = alloc::format!("{v}").len();
            lx.pos -= len;
        }
        Tok::Float(_) => {}
        Tok::Text(s) => lx.pos -= s.len() + 2,
        Tok::Star => lx.pos -= 1,
        Tok::Comma => lx.pos -= 1,
        Tok::Dot => lx.pos -= 1,
        Tok::Op(op) => {
            let len = match op {
                CmpOp::Le | CmpOp::Ne | CmpOp::Ge => 2,
                _ => 1,
            };
            lx.pos -= len;
        }
        Tok::Param => lx.pos -= 1,
        Tok::LParen => lx.pos -= 1,
        Tok::RParen => lx.pos -= 1,
        Tok::LBracket => lx.pos -= 1,
        Tok::Eof => {}
    }
}

fn parse_and(lx: &mut Lexer) -> Result<Expr, ParseError> {
    let mut left = parse_not(lx)?;
    loop {
        match lx.next_tok()? {
            Tok::Ident(kw) if eq_ignore_case(kw, "and") => {
                let right = parse_not(lx)?;
                left = Expr::And(Box::new(left), Box::new(right));
            }
            tok => {
                push_back(lx, tok);
                break;
            }
        }
    }
    Ok(left)
}

fn parse_not(lx: &mut Lexer) -> Result<Expr, ParseError> {
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "not") => {
            let inner = parse_not(lx)?;
            Ok(Expr::Not(Box::new(inner)))
        }
        tok => {
            push_back(lx, tok);
            parse_primary_expr(lx)
        }
    }
}

fn parse_primary_expr(lx: &mut Lexer) -> Result<Expr, ParseError> {
    let tok = lx.next_tok()?;
    match tok {
        Tok::Ident(kw) if eq_ignore_case(kw, "exists") => {
            expect_tok(lx, Tok::LParen, "'(' after EXISTS")?;
            expect_kw(lx, "select")?;
            let query = parse_select_inner(lx)?;
            expect_tok(lx, Tok::RParen, "')' after EXISTS subquery")?;
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
            expect_tok(lx, Tok::LParen, "'(' after aggregate function")?;
            let peek = lx.next_tok()?;
            let col = if peek == Tok::Star {
                expect_tok(lx, Tok::RParen, "')'")?;
                "*".to_string()
            } else {
                push_back(lx, peek);
                let c = expect_ident(lx, "column in aggregate")?;
                expect_tok(lx, Tok::RParen, "')'")?;
                c
            };
            // Check for optional comparison operator after aggregate, e.g.
            // `COUNT(*) > 1`. Uses the aggregate's default alias as the
            // column name so the executor can look it up in the result set.
            let alias = crate::executor::agg_default_alias(func, &col);
            match lx.next_tok()? {
                Tok::Op(op) => {
                    let value = parse_literal(lx)?;
                    Ok(Expr::Cmp {
                        column: alias,
                        op,
                        value,
                    })
                }
                tok => {
                    push_back(lx, tok);
                    Ok(Expr::Agg { func, column: col })
                }
            }
        }
        Tok::Ident(col) => {
            // Could be: col op val, col IS NULL, col LIKE ..., col IN (...),
            // col BETWEEN ... AND ..., or a keyword (shouldn't happen here).
            if is_kw(col) {
                return Err(ParseError(alloc::format!(
                    "unexpected keyword '{col}' in expression"
                )));
            }
            let mut col = col.to_string();
            let peek = lx.next_tok()?;
            if let Tok::Dot = peek {
                let field = expect_ident(lx, "field name after '.'")?;
                col = alloc::format!("{col}.{field}");
            } else {
                push_back(lx, peek);
            }
            let col: &str = &col;
            match lx.next_tok()? {
                Tok::Ident(kw) if eq_ignore_case(kw, "is") => {
                    let negated = match lx.next_tok()? {
                        Tok::Ident(kw) if eq_ignore_case(kw, "not") => {
                            expect_kw(lx, "null")?;
                            true
                        }
                        Tok::Ident(kw) if eq_ignore_case(kw, "null") => false,
                        _ => {
                            return Err(ParseError(
                                "expected NULL or NOT NULL after IS".to_string(),
                            ))
                        }
                    };
                    Ok(Expr::IsNull {
                        column: col.to_string(),
                        negated,
                    })
                }
                Tok::Ident(kw) if eq_ignore_case(kw, "like") => {
                    let pattern = match lx.next_tok()? {
                        Tok::Text(s) => s.to_string(),
                        _ => {
                            return Err(ParseError(
                                "expected pattern string after LIKE".to_string(),
                            ))
                        }
                    };
                    Ok(Expr::Like {
                        column: col.to_string(),
                        pattern,
                    })
                }
                Tok::Ident(kw) if eq_ignore_case(kw, "between") => {
                    let low = expect_int(lx, "BETWEEN low")?;
                    expect_kw(lx, "and")?;
                    let high = expect_int(lx, "BETWEEN high")?;
                    Ok(Expr::BetweenInt {
                        column: col.to_string(),
                        low,
                        high,
                    })
                }
                Tok::Ident(kw) if eq_ignore_case(kw, "in") => {
                    expect_tok(lx, Tok::LParen, "'(' after IN")?;
                    let peek = lx.next_tok()?;
                    if let Tok::Ident(k2) = &peek {
                        if eq_ignore_case(k2, "select") {
                            let query = parse_select_inner(lx)?;
                            expect_tok(lx, Tok::RParen, "')' after IN subquery")?;
                            return Ok(Expr::InSubquery {
                                column: col.to_string(),
                                query: Box::new(query),
                            });
                        }
                    }
                    push_back(lx, peek);
                    let mut values = Vec::new();
                    loop {
                        values.push(expect_int(lx, "value in IN list")?);
                        match lx.next_tok()? {
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
                        column: col.to_string(),
                        values,
                    })
                }
                Tok::Op(op) => {
                    // Check for NULL on the right side.
                    match lx.next_tok()? {
                        Tok::Ident(kw) if eq_ignore_case(kw, "null") => {
                            // `col = NULL` / `col != NULL` are non-standard but
                            // commonly accepted shorthand for IS [NOT] NULL.
                            match op {
                                CmpOp::Eq => Ok(Expr::IsNull {
                                    column: col.to_string(),
                                    negated: false,
                                }),
                                CmpOp::Ne => Ok(Expr::IsNull {
                                    column: col.to_string(),
                                    negated: true,
                                }),
                                _ => Err(ParseError(
                                    "comparison with NULL requires IS / IS NOT".to_string(),
                                )),
                            }
                        }
                        Tok::Int(v) => Ok(Expr::Cmp {
                            column: col.to_string(),
                            op,
                            value: Literal::Int(v),
                        }),
                        Tok::Text(s) => Ok(Expr::Cmp {
                            column: col.to_string(),
                            op,
                            value: Literal::Text(s.to_string()),
                        }),
                        Tok::Ident(right) => {
                            let mut right = right.to_string();
                            let peek = lx.next_tok()?;
                            if let Tok::Dot = peek {
                                let field = expect_ident(lx, "field name after '.'")?;
                                right = alloc::format!("{right}.{field}");
                            } else {
                                push_back(lx, peek);
                            }
                            Ok(Expr::CmpColumn {
                                left: col.to_string(),
                                op,
                                right,
                            })
                        }
                        Tok::LParen => {
                            expect_kw(lx, "select")?;
                            let query = parse_select_inner(lx)?;
                            expect_tok(lx, Tok::RParen, "')' after scalar subquery")?;
                            Ok(Expr::ScalarCmp {
                                column: col.to_string(),
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
            // Parenthesized sub-expression.
            let inner = parse_expr(lx)?;
            expect_tok(lx, Tok::RParen, "')'")?;
            Ok(inner)
        }
        _ => Err(ParseError(
            "expected column name or '(' in expression".to_string(),
        )),
    }
}

fn expect_int(lx: &mut Lexer, what: &str) -> Result<i64, ParseError> {
    match lx.next_tok()? {
        Tok::Int(v) => Ok(v),
        _ => Err(ParseError(alloc::format!("expected integer {what}"))),
    }
}

fn expect_tok(lx: &mut Lexer, expected: Tok, what: &str) -> Result<(), ParseError> {
    let tok = lx.next_tok()?;
    if tok == expected {
        Ok(())
    } else {
        Err(ParseError(alloc::format!("expected {what}")))
    }
}

/// Parse a table reference: either a plain name or `(SELECT ...) AS alias`.
fn parse_table_ref(lx: &mut Lexer) -> Result<TableRef, ParseError> {
    let tok = lx.next_tok()?;
    match tok {
        Tok::LParen => {
            // Subquery: (SELECT ...) AS alias
            // Consume the SELECT keyword before calling parse_select_inner.
            match lx.next_tok()? {
                Tok::Ident(kw) if eq_ignore_case(kw, "select") => {}
                _ => return Err(ParseError("expected SELECT in subquery".to_string())),
            }
            let query = parse_select_inner(lx)?;
            expect_tok(lx, Tok::RParen, "')' after subquery")?;
            let alias = match lx.next_tok()? {
                Tok::Ident(kw) if eq_ignore_case(kw, "as") => expect_ident(lx, "subquery alias")?,
                t => {
                    push_back(lx, t);
                    return Err(ParseError("subquery must have an AS alias".to_string()));
                }
            };
            Ok(TableRef::Subquery {
                query: Box::new(query),
                alias,
            })
        }
        Tok::Ident(name) => {
            let alias = match lx.next_tok()? {
                Tok::Ident(kw) if eq_ignore_case(kw, "as") => {
                    Some(expect_ident(lx, "table alias")?)
                }
                other => {
                    push_back(lx, other);
                    None
                }
            };
            Ok(TableRef::Named {
                name: name.to_string(),
                alias,
            })
        }
        _ => Err(ParseError("expected table name or '('".to_string())),
    }
}

// ---------------------------------------------------------------------------
// Vector literal
// ---------------------------------------------------------------------------

fn parse_vector_literal(lx: &mut Lexer) -> Result<Literal, ParseError> {
    // The '[' has already been consumed by the lexer (LBracket token).
    // Skip any whitespace and parse comma-separated floats until ']'.
    let b = lx.bytes();
    let mut vals = Vec::new();
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

fn parse_literal(lx: &mut Lexer) -> Result<Literal, ParseError> {
    match lx.next_tok()? {
        Tok::Int(v) => Ok(Literal::Int(v)),
        Tok::Text(s) => Ok(Literal::Text(s.to_string())),
        Tok::LBracket => parse_vector_literal(lx),
        Tok::Ident(kw) if eq_ignore_case(kw, "null") => Ok(Literal::Null),
        _ => Err(ParseError("expected a value".to_string())),
    }
}

// ---------------------------------------------------------------------------
// CREATE VIEW
// ---------------------------------------------------------------------------

fn parse_create_view(lx: &mut Lexer) -> Result<CreateViewStatement, ParseError> {
    let name = expect_ident(lx, "view name")?;
    expect_kw(lx, "as")?;
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "select") => {
            let query = parse_select_inner(lx)?;
            if lx.next_tok()? != Tok::Eof {
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

fn parse_create_table(lx: &mut Lexer) -> Result<CreateTableStatement, ParseError> {
    expect_kw(lx, "table")?;
    let table = expect_ident(lx, "table name")?;
    expect_tok(lx, Tok::LParen, "'('")?;
    let mut columns = Vec::new();
    loop {
        let name = expect_ident(lx, "column name")?;
        let ctype = match lx.next_tok()? {
            Tok::Ident(kw) if eq_ignore_case(kw, "int") || eq_ignore_case(kw, "integer") => {
                ColumnType::Int
            }
            Tok::Ident(kw) if eq_ignore_case(kw, "text") => ColumnType::Text,
            Tok::Ident(kw) if eq_ignore_case(kw, "vector") => {
                // Optional [N] dimension hint (ignored for now).
                let b = lx.bytes();
                if lx.pos < b.len() && b[lx.pos] == b'[' {
                    lx.pos += 1;
                    // Skip until ']'.
                    while lx.pos < b.len() && b[lx.pos] != b']' {
                        lx.pos += 1;
                    }
                    if lx.pos < b.len() {
                        lx.pos += 1; // skip ']'
                    }
                }
                ColumnType::Vector
            }
            _ => {
                return Err(ParseError(
                    "expected column type (INT, TEXT, VECTOR)".to_string(),
                ))
            }
        };
        columns.push(ColumnDef { name, ctype });
        match lx.next_tok()? {
            Tok::Comma => continue,
            Tok::RParen => break,
            _ => return Err(ParseError("expected ',' or ')'".to_string())),
        }
    }
    Ok(CreateTableStatement { table, columns })
}

// ---------------------------------------------------------------------------
// INSERT
// ---------------------------------------------------------------------------

fn parse_insert(mut lx: Lexer) -> Result<InsertStatement, ParseError> {
    expect_kw(&mut lx, "into")?;
    let table = expect_ident(&mut lx, "table name")?;

    let mut columns = Vec::new();
    let mut t = lx.next_tok()?;
    if let Tok::LParen = t {
        loop {
            let col = expect_ident(&mut lx, "column name")?;
            columns.push(col);
            match lx.next_tok()? {
                Tok::Comma => continue,
                Tok::RParen => break,
                _ => return Err(ParseError("expected ',' or ')'".to_string())),
            }
        }
        t = lx.next_tok()?;
    }

    match t {
        Tok::Ident(kw) if eq_ignore_case(kw, "values") => {}
        _ => return Err(ParseError("expected VALUES".to_string())),
    }
    expect_tok(&mut lx, Tok::LParen, "'(' before values")?;
    let mut values = Vec::new();
    loop {
        values.push(parse_literal(&mut lx)?);
        match lx.next_tok()? {
            Tok::Comma => continue,
            Tok::RParen => break,
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

// ---------------------------------------------------------------------------
// UPDATE
// ---------------------------------------------------------------------------

fn parse_update(mut lx: Lexer) -> Result<UpdateStatement, ParseError> {
    let table = expect_ident(&mut lx, "table name")?;
    expect_kw(&mut lx, "set")?;
    let mut assignments = Vec::new();
    loop {
        let column = expect_ident(&mut lx, "column name")?;
        expect_tok(&mut lx, Tok::Op(CmpOp::Eq), "'=' in assignment")?;
        let value = parse_literal(&mut lx)?;
        assignments.push(Assignment { column, value });
        match lx.next_tok()? {
            Tok::Comma => continue,
            tok => {
                push_back(&mut lx, tok);
                break;
            }
        }
    }
    // Optional WHERE.
    let t = lx.next_tok()?;
    let filter = if let Tok::Ident(kw) = &t {
        if eq_ignore_case(kw, "where") {
            Some(parse_expr(&mut lx)?)
        } else {
            push_back(&mut lx, t);
            None
        }
    } else {
        push_back(&mut lx, t);
        None
    };
    if lx.next_tok()? != Tok::Eof {
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

fn parse_delete(mut lx: Lexer) -> Result<DeleteStatement, ParseError> {
    expect_kw(&mut lx, "from")?;
    let table = expect_ident(&mut lx, "table name")?;
    let t = lx.next_tok()?;
    let filter = if let Tok::Ident(kw) = &t {
        if eq_ignore_case(kw, "where") {
            Some(parse_expr(&mut lx)?)
        } else {
            None
        }
    } else {
        None
    };
    if lx.next_tok()? != Tok::Eof {
        return Err(ParseError("trailing tokens after statement".to_string()));
    }
    Ok(DeleteStatement { table, filter })
}

// ---------------------------------------------------------------------------
// SELECT
// ---------------------------------------------------------------------------

fn parse_select_inner(lx: &mut Lexer) -> Result<SelectStatement, ParseError> {
    // Columns or *.
    let mut columns = Vec::new();
    let mut star = false;
    let mut aggregates = Vec::new();
    let mut t = lx.next_tok()?;
    if t == Tok::Star {
        star = true;
        t = lx.next_tok()?;
    } else {
        loop {
            match t {
                Tok::Ident(name) => {
                    let col_name = name.to_string();
                    // Check for aggregate function: COUNT/SUM/AVG/MIN/MAX ( ... )
                    if let Some(agg) = match col_name.to_ascii_lowercase().as_str() {
                        "count" => Some(AggregateFunc::Count),
                        "sum" => Some(AggregateFunc::Sum),
                        "avg" => Some(AggregateFunc::Avg),
                        "min" => Some(AggregateFunc::Min),
                        "max" => Some(AggregateFunc::Max),
                        _ => None,
                    } {
                        let next = lx.next_tok()?;
                        if let Tok::LParen = next {
                            let peek = lx.next_tok()?;
                            let inner = if peek == Tok::Star {
                                expect_tok(lx, Tok::RParen, "')'")?;
                                "*".to_string()
                            } else {
                                push_back(lx, peek);
                                let c = expect_ident(lx, "column in aggregate")?;
                                expect_tok(lx, Tok::RParen, "')'")?;
                                c
                            };
                            let alias = match lx.next_tok()? {
                                Tok::Ident(kw) if eq_ignore_case(kw, "as") => {
                                    expect_ident(lx, "alias")?
                                }
                                tok => {
                                    push_back(lx, tok);
                                    alloc::format!("{agg:?}").to_lowercase()
                                }
                            };
                            aggregates.push((alias.clone(), agg, inner));
                            columns.push(alias);
                            match lx.next_tok()? {
                                Tok::Comma => {
                                    t = lx.next_tok()?;
                                    continue;
                                }
                                tok => {
                                    push_back(lx, tok);
                                }
                            }
                        } else {
                            push_back(lx, next);
                            columns.push(col_name);
                        }
                    } else {
                        // Regular column — allow a qualified name (`t.col`),
                        // then check for alias: col AS name
                        let mut col_name = col_name;
                        let mut next = lx.next_tok()?;
                        if let Tok::Dot = next {
                            let field = expect_ident(lx, "field name after '.'")?;
                            col_name = alloc::format!("{col_name}.{field}");
                            next = lx.next_tok()?;
                        }
                        if let Tok::Ident(kw) = &next {
                            if eq_ignore_case(kw, "as") {
                                let alias = expect_ident(lx, "alias")?;
                                columns.push(alias);
                            } else {
                                push_back(lx, next);
                                columns.push(col_name);
                            }
                        } else {
                            push_back(lx, next);
                            columns.push(col_name);
                        }
                    }
                }
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
    let table = parse_table_ref(lx)?;

    // Optional JOINs.
    let mut joins = Vec::new();
    loop {
        let t = lx.next_tok()?;
        if let Tok::Ident(kw) = &t {
            if eq_ignore_case(kw, "join") {
                let join_table = parse_table_ref(lx)?;
                expect_kw(lx, "on")?;
                let left_col = expect_ident(lx, "left column")?;
                expect_tok(lx, Tok::Op(CmpOp::Eq), "'=' in ON clause")?;
                let right_col = expect_ident(lx, "right column")?;
                joins.push(Join {
                    jtype: JoinType::Inner,
                    table: join_table,
                    left_col,
                    right_col,
                });
                continue;
            }
        }
        push_back(lx, t);
        break;
    }

    let mut filter = None;
    let mut limit = None;
    let mut order_by_cosine = None;
    let mut group_by = Vec::new();
    let mut having = None;
    let mut order_by = Vec::new();

    // Optional WHERE / GROUP BY / ORDER BY / LIMIT.
    let mut t = lx.next_tok()?;
    loop {
        match t {
            Tok::Ident(kw) if eq_ignore_case(kw, "where") => {
                if filter.is_some() {
                    return Err(ParseError("duplicate WHERE".to_string()));
                }
                filter = Some(parse_expr(lx)?);
                t = lx.next_tok()?;
            }
            Tok::Ident(kw) if eq_ignore_case(kw, "group") => {
                expect_kw(lx, "by")?;
                loop {
                    let col = expect_ident(lx, "column in GROUP BY")?;
                    group_by.push(col);
                    match lx.next_tok()? {
                        Tok::Comma => continue,
                        tok => {
                            push_back(lx, tok);
                            break;
                        }
                    }
                }
                t = lx.next_tok()?;
            }
            Tok::Ident(kw) if eq_ignore_case(kw, "having") => {
                if having.is_some() {
                    return Err(ParseError("duplicate HAVING".to_string()));
                }
                having = Some(parse_expr(lx)?);
                t = lx.next_tok()?;
            }
            Tok::Ident(kw) if eq_ignore_case(kw, "order") => {
                expect_kw(lx, "by")?;
                let first_col = lx.next_tok()?;
                // Check if this is ORDER BY cosine(...) — legacy vector path.
                if let Tok::Ident(name) = &first_col {
                    if eq_ignore_case(name, "cosine") {
                        // Re-parse the cosine path.
                        expect_tok(lx, Tok::LParen, "'(' after cosine")?;
                        let column = expect_ident(lx, "embedding column")?;
                        expect_tok(lx, Tok::Comma, "',' in cosine(...)")?;
                        let _param = match lx.next_tok()? {
                            Tok::Param => 1,
                            _ => return Err(ParseError("expected '?' query vector".to_string())),
                        };
                        expect_tok(lx, Tok::RParen, "')' after cosine(...)")?;
                        expect_kw(lx, "limit")?;
                        let k = expect_int(lx, "LIMIT k")?;
                        order_by_cosine = Some(OrderByCosine {
                            column,
                            param: 1,
                            k: k as u64,
                        });
                        t = lx.next_tok()?;
                        continue;
                    }
                    // Regular ORDER BY: col [ASC|DESC]
                    let mut descending = false;
                    let col = name.to_string();
                    let next = lx.next_tok()?;
                    match next {
                        Tok::Ident(kw) if eq_ignore_case(kw, "asc") => {
                            descending = false;
                        }
                        Tok::Ident(kw) if eq_ignore_case(kw, "desc") => {
                            descending = true;
                        }
                        _ => {
                            push_back(lx, next);
                        }
                    }
                    order_by.push(OrderBy {
                        column: col,
                        descending,
                    });
                    // Continue parsing more ORDER BY columns.
                    loop {
                        let next = lx.next_tok()?;
                        if let Tok::Comma = next {
                            let col_tok = lx.next_tok()?;
                            if let Tok::Ident(name) = col_tok {
                                let mut desc = false;
                                let nxt = lx.next_tok()?;
                                match nxt {
                                    Tok::Ident(kw) if eq_ignore_case(kw, "desc") => {
                                        desc = true;
                                    }
                                    Tok::Ident(kw) if eq_ignore_case(kw, "asc") => {}
                                    _ => push_back(lx, nxt),
                                }
                                order_by.push(OrderBy {
                                    column: name.to_string(),
                                    descending: desc,
                                });
                            } else {
                                return Err(ParseError(
                                    "expected column name in ORDER BY".to_string(),
                                ));
                            }
                        } else {
                            push_back(lx, next);
                            break;
                        }
                    }
                    t = lx.next_tok()?;
                    continue;
                }
                return Err(ParseError(
                    "only ORDER BY <col> and ORDER BY cosine(...) are supported".to_string(),
                ));
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

    // Leave the terminating token (`Eof` for a top-level statement, `)` for a
    // subquery) for the caller to check — this function is also invoked
    // recursively for derived tables, where `Eof` is never reached.
    push_back(lx, t);

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

/// Parse a `WITH` clause: `name AS (SELECT ...) [, name2 AS (SELECT ...)]`.
/// Returns the list of CTEs. Called when the top-level token is `WITH`.
fn parse_with_clause(lx: &mut Lexer) -> Result<Vec<CTE>, ParseError> {
    let mut ctes = Vec::new();
    loop {
        let name = expect_ident(lx, "CTE name")?;
        expect_kw(lx, "as")?;
        expect_tok(lx, Tok::LParen, "'(' after AS in CTE")?;
        match lx.next_tok()? {
            Tok::Ident(kw) if eq_ignore_case(kw, "select") => {}
            _ => return Err(ParseError("expected SELECT in CTE".to_string())),
        }
        let query = parse_select_inner(lx)?;
        expect_tok(lx, Tok::RParen, "')' after CTE subquery")?;
        ctes.push(CTE { name, query });
        // Check for comma (more CTEs) or end of WITH clause.
        match lx.next_tok()? {
            Tok::Comma => continue,
            tok => {
                push_back(lx, tok);
                break;
            }
        }
    }
    Ok(ctes)
}

// ---------------------------------------------------------------------------
// ALTER TABLE
// ---------------------------------------------------------------------------

fn parse_alter_table(lx: &mut Lexer) -> Result<AlterTableStatement, ParseError> {
    let table = expect_ident(lx, "table name")?;
    let op = match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "add") => {
            expect_kw(lx, "column")?;
            let name = expect_ident(lx, "column name")?;
            let ctype = match lx.next_tok()? {
                Tok::Ident(kw) if eq_ignore_case(kw, "int") => ColumnType::Int,
                Tok::Ident(kw) if eq_ignore_case(kw, "text") => ColumnType::Text,
                Tok::Ident(kw) if eq_ignore_case(kw, "vector") => {
                    // Optional [N] dimension hint, ignored.
                    if let Tok::LBracket = lx.next_tok()? {
                        // Skip digits and consume ']'.
                        while lx.pos < lx.bytes().len() && lx.bytes()[lx.pos].is_ascii_digit() {
                            lx.pos += 1;
                        }
                        if lx.pos < lx.bytes().len() && lx.bytes()[lx.pos] == b']' {
                            lx.pos += 1;
                        }
                    } else {
                        push_back(lx, Tok::Ident("vector"));
                    }
                    ColumnType::Vector
                }
                _ => {
                    return Err(ParseError(
                        "expected column type (INT, TEXT, VECTOR)".to_string(),
                    ))
                }
            };
            AlterTableOp::AddColumn(ColumnDef { name, ctype })
        }
        Tok::Ident(kw) if eq_ignore_case(kw, "drop") => {
            expect_kw(lx, "column")?;
            let name = expect_ident(lx, "column name")?;
            AlterTableOp::DropColumn(name)
        }
        Tok::Ident(kw) if eq_ignore_case(kw, "rename") => {
            expect_kw(lx, "column")?;
            let old_name = expect_ident(lx, "old column name")?;
            expect_kw(lx, "to")?;
            let new_name = expect_ident(lx, "new column name")?;
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

// ---------------------------------------------------------------------------
// Top-level dispatch
// ---------------------------------------------------------------------------

pub fn parse_statement(input: &str) -> Result<Statement, ParseError> {
    let mut lx = Lexer::new(input);
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "with") => {
            let ctes = parse_with_clause(&mut lx)?;
            // After WITH, expect SELECT.
            match lx.next_tok()? {
                Tok::Ident(kw) if eq_ignore_case(kw, "select") => {}
                _ => return Err(ParseError("expected SELECT after WITH".to_string())),
            }
            let mut stmt = parse_select_inner(&mut lx)?;
            stmt.with_ctes = ctes;
            if lx.next_tok()? != Tok::Eof {
                return Err(ParseError("trailing tokens after statement".to_string()));
            }
            Ok(Statement::Select(stmt))
        }
        Tok::Ident(kw) if eq_ignore_case(kw, "select") => {
            let stmt = parse_select_inner(&mut lx)?;
            if lx.next_tok()? != Tok::Eof {
                return Err(ParseError("trailing tokens after statement".to_string()));
            }
            Ok(Statement::Select(stmt))
        }
        Tok::Ident(kw) if eq_ignore_case(kw, "insert") => parse_insert(lx).map(Statement::Insert),
        Tok::Ident(kw) if eq_ignore_case(kw, "update") => parse_update(lx).map(Statement::Update),
        Tok::Ident(kw) if eq_ignore_case(kw, "delete") => parse_delete(lx).map(Statement::Delete),
        Tok::Ident(kw) if eq_ignore_case(kw, "create") => {
            let sub = lx.next_tok()?;
            match &sub {
                Tok::Ident(s) if eq_ignore_case(s, "table") => {
                    push_back(&mut lx, sub);
                    parse_create_table(&mut lx).map(Statement::CreateTable)
                }
                Tok::Ident(s) if eq_ignore_case(s, "view") => {
                    parse_create_view(&mut lx).map(Statement::CreateView)
                }
                _ => Err(ParseError(
                    "expected TABLE or VIEW after CREATE".to_string(),
                )),
            }
        }
        Tok::Ident(kw) if eq_ignore_case(kw, "drop") => match lx.next_tok()? {
            Tok::Ident(s) if eq_ignore_case(s, "view") => {
                let name = expect_ident(&mut lx, "view name")?;
                if lx.next_tok()? != Tok::Eof {
                    return Err(ParseError("trailing tokens after statement".to_string()));
                }
                Ok(Statement::DropView(name))
            }
            _ => Err(ParseError("expected VIEW after DROP".to_string())),
        },
        Tok::Ident(kw) if eq_ignore_case(kw, "alter") => {
            expect_kw(&mut lx, "table")?;
            let stmt = parse_alter_table(&mut lx)?;
            if lx.next_tok()? != Tok::Eof {
                return Err(ParseError("trailing tokens after statement".to_string()));
            }
            Ok(Statement::AlterTable(stmt))
        }
        Tok::Ident(kw) if eq_ignore_case(kw, "begin") => Ok(Statement::Begin),
        Tok::Ident(kw) if eq_ignore_case(kw, "commit") => Ok(Statement::Commit),
        Tok::Ident(kw) if eq_ignore_case(kw, "rollback") => Ok(Statement::Rollback),
        _ => Err(ParseError(
            "expected SELECT, INSERT, UPDATE, DELETE, CREATE TABLE, CREATE VIEW, DROP VIEW, \
             ALTER TABLE, BEGIN, COMMIT, or ROLLBACK"
                .to_string(),
        )),
    }
}

/// Convenience: parse a standalone SELECT (with optional WITH clause).
pub fn parse_select(input: &str) -> Result<SelectStatement, ParseError> {
    let mut lx = Lexer::new(input);
    let ctes = match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "with") => parse_with_clause(&mut lx)?,
        tok => {
            push_back(&mut lx, tok);
            Vec::new()
        }
    };
    match lx.next_tok()? {
        Tok::Ident(kw) if eq_ignore_case(kw, "select") => {}
        _ => return Err(ParseError("expected SELECT".to_string())),
    }
    let mut stmt = parse_select_inner(&mut lx)?;
    stmt.with_ctes = ctes;
    if lx.next_tok()? != Tok::Eof {
        return Err(ParseError("trailing tokens after statement".to_string()));
    }
    Ok(stmt)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_star_select() {
        let s = parse_select("SELECT * FROM users").unwrap();
        assert!(s.star);
        assert_eq!(
            s.table,
            TableRef::Named {
                name: "users".into(),
                alias: None
            }
        );
        assert!(s.filter.is_none());
        assert!(s.limit.is_none());
    }

    #[test]
    fn parses_columns_where_limit() {
        let s = parse_select("SELECT id, name FROM t WHERE age >= 18 LIMIT 5").unwrap();
        assert!(!s.star);
        assert_eq!(s.columns, alloc::vec!["id".to_string(), "name".to_string()]);
        assert_eq!(
            s.table,
            TableRef::Named {
                name: "t".into(),
                alias: None
            }
        );
        assert!(s.filter.is_some());
        assert_eq!(s.limit, Some(5));
    }

    #[test]
    fn parses_and_or_where() {
        let s = parse_select("SELECT * FROM t WHERE a = 1 AND b = 2 OR c = 3").unwrap();
        let f = s.filter.unwrap();
        // Should be (a=1 AND b=2) OR c=3.
        assert!(matches!(f, Expr::Or(_, _)));
    }

    #[test]
    fn parses_is_null() {
        let s = parse_select("SELECT * FROM t WHERE x IS NULL").unwrap();
        assert!(matches!(
            s.filter,
            Some(Expr::IsNull { ref column, negated: false }) if column == "x"
        ));
    }

    #[test]
    fn parses_is_not_null() {
        let s = parse_select("SELECT * FROM t WHERE x IS NOT NULL").unwrap();
        assert!(matches!(
            s.filter,
            Some(Expr::IsNull { ref column, negated: true }) if column == "x"
        ));
    }

    #[test]
    fn parses_not_equals_null_as_is_not_null() {
        let s = parse_select("SELECT * FROM t WHERE x != NULL").unwrap();
        assert!(matches!(
            s.filter,
            Some(Expr::IsNull { ref column, negated: true }) if column == "x"
        ));
    }

    #[test]
    fn parses_equals_null_as_is_null() {
        let s = parse_select("SELECT * FROM t WHERE x = NULL").unwrap();
        assert!(matches!(
            s.filter,
            Some(Expr::IsNull { ref column, negated: false }) if column == "x"
        ));
    }

    #[test]
    fn parses_not_prefix() {
        let s = parse_select("SELECT * FROM t WHERE NOT x IS NULL").unwrap();
        assert!(matches!(s.filter, Some(Expr::Not(_))));
    }

    #[test]
    fn parses_text_comparison() {
        let s = parse_select("SELECT * FROM t WHERE name = 'bob'").unwrap();
        assert!(matches!(
            s.filter,
            Some(Expr::Cmp { ref column, value: Literal::Text(ref v), .. })
                if column == "name" && v == "bob"
        ));
    }

    #[test]
    fn parses_create_view() {
        let s = parse_statement("CREATE VIEW adults AS SELECT * FROM t WHERE age >= 18").unwrap();
        match s {
            Statement::CreateView(cv) => {
                assert_eq!(cv.name, "adults");
                assert_eq!(
                    cv.query.table,
                    TableRef::Named {
                        name: "t".into(),
                        alias: None
                    }
                );
                assert!(cv.query.filter.is_some());
            }
            _ => panic!("expected CreateView"),
        }
    }

    #[test]
    fn parses_drop_view() {
        let s = parse_statement("DROP VIEW adults").unwrap();
        assert!(matches!(s, Statement::DropView(name) if name == "adults"));
    }

    #[test]
    fn parses_like() {
        let s = parse_select("SELECT * FROM t WHERE name LIKE '%alice%'").unwrap();
        assert!(matches!(
            s.filter,
            Some(Expr::Like { column, pattern }) if column == "name" && pattern == "%alice%"
        ));
    }

    #[test]
    fn parses_in_list() {
        let s = parse_select("SELECT * FROM t WHERE x IN (1, 2, 3)").unwrap();
        assert!(matches!(
            s.filter,
            Some(Expr::InInt { column, values }) if column == "x" && values == alloc::vec![1, 2, 3]
        ));
    }

    #[test]
    fn parses_between() {
        let s = parse_select("SELECT * FROM t WHERE x BETWEEN 10 AND 20").unwrap();
        assert!(matches!(
            s.filter,
            Some(Expr::BetweenInt { column, low, high }) if column == "x" && low == 10 && high == 20
        ));
    }

    #[test]
    fn parses_group_by() {
        let s = parse_select("SELECT dept, COUNT(*) FROM t GROUP BY dept").unwrap();
        assert_eq!(s.group_by, alloc::vec!["dept".to_string()]);
        assert_eq!(s.aggregates.len(), 1);
        assert_eq!(s.aggregates[0].1, AggregateFunc::Count);
    }

    #[test]
    fn parses_order_by_column() {
        let s = parse_select("SELECT * FROM t ORDER BY name DESC").unwrap();
        assert_eq!(s.order_by.len(), 1);
        assert_eq!(s.order_by[0].column, "name");
        assert!(s.order_by[0].descending);
    }

    #[test]
    fn parses_order_by_multiple() {
        let s = parse_select("SELECT * FROM t ORDER BY a ASC, b DESC").unwrap();
        assert_eq!(s.order_by.len(), 2);
        assert!(!s.order_by[0].descending);
        assert!(s.order_by[1].descending);
    }

    #[test]
    fn parses_join() {
        let s = parse_select("SELECT * FROM t1 JOIN t2 ON t1.id = t2.t1_id").unwrap();
        assert_eq!(s.joins.len(), 1);
        assert_eq!(
            s.joins[0].table,
            TableRef::Named {
                name: "t2".into(),
                alias: None
            }
        );
        assert_eq!(s.joins[0].left_col, "t1.id");
        assert_eq!(s.joins[0].right_col, "t2.t1_id");
    }

    #[test]
    fn parses_create_table() {
        let s = parse_statement("CREATE TABLE t (id INT, name TEXT, emb VECTOR[128])").unwrap();
        match s {
            Statement::CreateTable(ct) => {
                assert_eq!(ct.table, "t");
                assert_eq!(ct.columns.len(), 3);
                assert_eq!(ct.columns[0].ctype, ColumnType::Int);
                assert_eq!(ct.columns[1].ctype, ColumnType::Text);
                assert_eq!(ct.columns[2].ctype, ColumnType::Vector);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parses_begin_commit_rollback() {
        assert!(matches!(parse_statement("BEGIN"), Ok(Statement::Begin)));
        assert!(matches!(parse_statement("COMMIT"), Ok(Statement::Commit)));
        assert!(matches!(
            parse_statement("ROLLBACK"),
            Ok(Statement::Rollback)
        ));
    }

    #[test]
    fn parses_insert_with_null() {
        let s = parse_statement("INSERT INTO t (id, name) VALUES (1, NULL)").unwrap();
        if let Statement::Insert(i) = s {
            assert_eq!(i.values[1], Literal::Null);
        } else {
            panic!("expected Insert");
        }
    }

    #[test]
    fn parses_update_with_complex_where() {
        let s = parse_statement("UPDATE t SET x = 1 WHERE a > 5 AND b < 10").unwrap();
        if let Statement::Update(u) = s {
            assert!(u.filter.is_some());
        } else {
            panic!("expected Update");
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_select("UPDATE t SET x=1").is_err());
        assert!(parse_select("SELECT FROM t").is_err());
        assert!(parse_statement("XYZ").is_err());
    }

    #[test]
    fn all_comparison_operators() {
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
            match s.filter.unwrap() {
                Expr::Cmp { op: o, .. } => assert_eq!(o, op, "op {src}"),
                _ => panic!("expected Cmp for op {src}"),
            }
        }
    }

    #[test]
    fn parses_subquery_in_from() {
        let s = parse_statement("SELECT * FROM (SELECT id, name FROM t WHERE age >= 20) AS sub")
            .unwrap();
        if let Statement::Select(sel) = s {
            assert!(sel.star);
            match &sel.table {
                TableRef::Subquery { query, alias } => {
                    assert_eq!(alias, "sub");
                    assert_eq!(
                        query.table,
                        TableRef::Named {
                            name: "t".into(),
                            alias: None
                        }
                    );
                    assert!(query.filter.is_some());
                }
                _ => panic!("expected Subquery"),
            }
        } else {
            panic!("expected Select");
        }
    }

    #[test]
    fn parses_subquery_without_alias_errors() {
        let r = parse_statement("SELECT * FROM (SELECT id FROM t)");
        assert!(r.is_err());
    }

    #[test]
    fn parses_with_cte() {
        let s = parse_statement("WITH cte AS (SELECT id FROM t WHERE age > 18) SELECT * FROM cte")
            .unwrap();
        if let Statement::Select(sel) = s {
            assert_eq!(sel.with_ctes.len(), 1);
            assert_eq!(sel.with_ctes[0].name, "cte");
            assert!(sel.with_ctes[0].query.filter.is_some());
        } else {
            panic!("expected Select");
        }
    }

    #[test]
    fn parses_multiple_ctes() {
        let s = parse_statement(
            "WITH a AS (SELECT id FROM t), b AS (SELECT id FROM t) SELECT * FROM a",
        )
        .unwrap();
        if let Statement::Select(sel) = s {
            assert_eq!(sel.with_ctes.len(), 2);
            assert_eq!(sel.with_ctes[0].name, "a");
            assert_eq!(sel.with_ctes[1].name, "b");
        } else {
            panic!("expected Select");
        }
    }

    #[test]
    fn parses_exists_with_column_comparison() {
        let s = parse_statement(
            "SELECT id FROM t WHERE EXISTS (SELECT id FROM t AS inner_t WHERE inner_t.id < t.id)",
        )
        .unwrap();
        if let Statement::Select(sel) = s {
            assert!(sel.filter.is_some());
        } else {
            panic!("expected Select");
        }
    }

    #[test]
    fn parses_column_to_column() {
        let s = parse_statement("SELECT id FROM t WHERE id < age").unwrap();
        if let Statement::Select(sel) = s {
            assert!(sel.filter.is_some());
        } else {
            panic!("expected Select");
        }
    }

    #[test]
    fn parses_recursive_cte_errors() {
        // WITH RECURSIVE is not supported yet.
        let r = parse_statement("WITH RECURSIVE cte AS (SELECT 1) SELECT * FROM cte");
        assert!(r.is_err());
    }

    #[test]
    fn parses_having() {
        let s =
            parse_select("SELECT dept, COUNT(*) FROM t GROUP BY dept HAVING COUNT(*) > 1").unwrap();
        assert!(s.having.is_some());
        assert_eq!(s.group_by, alloc::vec!["dept".to_string()]);
    }
}
