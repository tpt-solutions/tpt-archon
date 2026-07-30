//! AST types produced by the parser: literals, expressions, and one struct
//! per statement kind (`SELECT`/`INSERT`/`UPDATE`/`DELETE`/DDL).

use alloc::boxed::Box;
use alloc::string::String;
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
    /// A floating-point literal.
    Float(f32),
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
    /// A comparison against a raw aggregate call, e.g. `HAVING COUNT(x) > 1`,
    /// before it's been resolved to whatever alias the matching SELECT-list
    /// aggregate actually got. `resolve_having_aliases` always rewrites this
    /// into a plain [`Expr::Cmp`] before a parsed `HAVING` is returned from
    /// `parse_select_inner_impl` — kept as its own variant (rather than
    /// eagerly collapsing into `Cmp` with a guessed alias, which is exactly
    /// the bug this exists to avoid) so that rewrite can tell "this came from
    /// an aggregate call" apart from "this is a column literally named after
    /// an aggregate's default alias."
    AggCmp {
        /// The aggregate function.
        func: AggregateFunc,
        /// The column argument (`*` for `COUNT(*)`).
        column: String,
        /// The comparison operator.
        op: CmpOp,
        /// The right-hand value.
        value: Literal,
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
    /// `BOOLEAN` — true/false.
    Boolean,
    /// `FLOAT` — 32-bit floating point.
    Float,
    /// `DOUBLE` — 64-bit floating point.
    Double,
    /// `NUMERIC` — fixed-point decimal (stored as i64 for now, A2 adds precision).
    Numeric,
    /// `TEXT` — UTF-8 text.
    Text,
    /// `VARCHAR(n)` — variable-length UTF-8 text (length limit enforced at DDL).
    Varchar(usize),
    /// `DATE` — calendar date (stored as i64 days since epoch for now, A2 adds real type).
    Date,
    /// `TIMESTAMP` — point in time (stored as i64 micros since epoch for now).
    Timestamp,
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
