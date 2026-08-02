//! `archon-sql` — interactive SQL REPL for the TPT Archon database engine.
//!
//! Usage:
//!   archon-sql              # interactive mode
//!   archon-sql -e "SQL;"    # execute a single statement and exit
//!   archon-sql --help       # show usage
//!   archon-sql --version    # show version
//!
//! Type `.help` for available commands, `.quit` to exit.

use std::io::{self, BufRead, Write};

use tpt_archon_relational::database::{Database, DbError};
use tpt_archon_relational::executor::Value;
use tpt_archon_relational::parser::parse_statement;

/// Result-set rendering format, toggled via `.mode`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Table,
    Csv,
    Json,
}

fn print_usage() {
    println!("archon-sql {}", env!("CARGO_PKG_VERSION"));
    println!("Interactive SQL shell for the TPT Archon database engine.");
    println!();
    println!("Usage:");
    println!("  archon-sql              Start the interactive REPL");
    println!("  archon-sql -e \"SQL;\"    Execute a single statement and exit");
    println!("  archon-sql --help       Show this help");
    println!("  archon-sql --version    Show the version");
    println!();
    println!("Once in the REPL, type .help for dot-commands, .quit to exit.");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("archon-sql {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.len() > 2 && args[1] == "-e" {
        let sql = &args[2];
        let mut db = Database::empty();
        run_one(&mut db, sql, OutputMode::Table);
        return;
    }

    println!("archon-sql — TPT Archon interactive SQL shell");
    println!("Type .help for commands, .quit to exit, or enter SQL terminated by ';'.\n");

    let mut db = Database::empty();
    let mut mode = OutputMode::Table;
    let stdin = io::stdin();
    let mut reader = stdin.lock();

    loop {
        print!("archon> ");
        let _ = io::stdout().flush();

        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.is_empty() {
            println!("Bye.");
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('.') {
            handle_dot_command(&mut db, &mut mode, trimmed);
            continue;
        }

        let mut sql = line.clone();
        while !sql.trim_end().ends_with(';') {
            print!("  ...> ");
            let _ = io::stdout().flush();
            let mut continuation = String::new();
            if reader.read_line(&mut continuation).is_err() || continuation.is_empty() {
                println!("Bye.");
                return;
            }
            sql.push_str(&continuation);
        }

        run_one(&mut db, &sql, mode);
    }
}

fn run_one(db: &mut Database, sql: &str, mode: OutputMode) {
    let sql = sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return;
    }

    match parse_statement(sql) {
        Ok(stmt) => {
            let start = std::time::Instant::now();
            match db.execute(&stmt, &[]) {
                Ok(rs) => {
                    let elapsed = start.elapsed();
                    if !rs.columns.is_empty() {
                        print_result_set(&rs.columns, &rs.rows, mode);
                    } else {
                        println!("OK");
                    }
                    println!("({:.3?})\n", elapsed);
                }
                Err(e) => {
                    eprintln!("Error: {}\n", fmt_db_error(&e));
                }
            }
        }
        Err(e) => {
            eprintln!("Parse error: {}\n", e.0);
        }
    }
}

fn print_result_set(columns: &[String], rows: &[Vec<Value>], mode: OutputMode) {
    match mode {
        OutputMode::Table => print_result_set_table(columns, rows),
        OutputMode::Csv => print_result_set_csv(columns, rows),
        OutputMode::Json => print_result_set_json(columns, rows),
    }
}

fn print_result_set_csv(columns: &[String], rows: &[Vec<Value>]) {
    let escape_csv = |s: String| {
        if s.contains(['"', ',', '\n']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s
        }
    };
    println!(
        "{}",
        columns
            .iter()
            .map(|c| escape_csv(c.clone()))
            .collect::<Vec<_>>()
            .join(",")
    );
    for row in rows {
        let cells: Vec<String> = row.iter().map(|v| escape_csv(display_value(v))).collect();
        println!("{}", cells.join(","));
    }
}

fn print_result_set_json(columns: &[String], rows: &[Vec<Value>]) {
    let escape_json = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let objects: Vec<String> = rows
        .iter()
        .map(|row| {
            let fields: Vec<String> = columns
                .iter()
                .zip(row.iter())
                .map(|(col, val)| {
                    let value_str = match val {
                        Value::Int(n) => n.to_string(),
                        Value::Null => "null".to_string(),
                        other => format!("\"{}\"", escape_json(&display_value(other))),
                    };
                    format!("\"{}\":{}", escape_json(col), value_str)
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        })
        .collect();
    println!("[{}]", objects.join(","));
}

fn print_result_set_table(columns: &[String], rows: &[Vec<Value>]) {
    if rows.is_empty() {
        println!("(0 rows)");
        return;
    }

    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, val) in row.iter().enumerate() {
            let w = display_value(val).len();
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }

    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:>width$}", c, width = widths[i]))
        .collect();
    println!("{}", header.join(" | "));

    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", sep.join("-+-"));

    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, val)| format!("{:>width$}", display_value(val), width = widths[i]))
            .collect();
        println!("{}", cells.join(" | "));
    }
    println!("({} rows)", rows.len());
}

fn display_value(val: &Value) -> String {
    match val {
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Vector(v) => {
            let inner: Vec<String> = v.iter().map(|f| format!("{:.4}", f)).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Null => "NULL".to_string(),
    }
}

fn fmt_db_error(e: &DbError) -> String {
    match e {
        DbError::UnknownColumn(c) => format!("unknown column '{c}'"),
        DbError::TypeMismatch => "type mismatch".to_string(),
        DbError::ColumnTypeMismatch(c) => format!("type mismatch for column '{c}'"),
        DbError::ArityMismatch => "arity mismatch".to_string(),
        DbError::NotAVectorColumn(c) => format!("column '{c}' is not a vector column"),
        DbError::MissingParam => "missing query parameter".to_string(),
        DbError::RowNotFound(id) => format!("row {id} not found"),
        DbError::CorruptRow(id) => format!("corrupt row at id {id}"),
        DbError::UnknownTable(t) => format!("unknown table '{t}'"),
        DbError::TransactionError(m) => format!("transaction error: {m}"),
        DbError::TableAlreadyExists(t) => format!("table '{t}' already exists"),
        DbError::ViewAlreadyExists(v) => format!("view '{v}' already exists"),
        DbError::UnknownView(v) => format!("unknown view '{v}'"),
        DbError::RecursiveView(v) => format!("view '{v}' cannot reference itself"),
        DbError::Unsupported(m) => format!("unsupported: {m}"),
        DbError::SubqueryCardinality(m) => format!("subquery error: {m}"),
        DbError::ColumnCountMismatch => {
            "each UNION/INTERSECT/EXCEPT query must have the same number of columns".to_string()
        }
        DbError::Exec(e) => format!("execution error: {e:?}"),
    }
}

fn handle_dot_command(db: &mut Database, mode: &mut OutputMode, cmd: &str) {
    match cmd {
        ".quit" | ".q" | ".exit" => {
            println!("Bye.");
            std::process::exit(0);
        }
        ".help" => {
            println!("Dot commands:");
            println!("  .tables              List all tables");
            println!("  .schema TBL          Show columns for a table");
            println!("  .mode table|csv|json Set result-set output format (default: table)");
            #[cfg(feature = "sqlite-import")]
            println!("  .import FILE [TABLE] Import a SQLite .sqlite file");
            println!("  .quit                Exit the REPL");
            println!("  .help                Show this help");
            println!();
            println!("SQL:");
            println!("  CREATE TABLE name (col TYPE, ...)");
            println!("  SELECT ... FROM ... WHERE ... GROUP BY ... ORDER BY ... LIMIT n");
            println!("  INSERT INTO name (cols) VALUES (...)");
            println!("  UPDATE name SET col = val WHERE ...");
            println!("  DELETE FROM name WHERE ...");
            println!("  BEGIN / COMMIT / ROLLBACK");
            println!();
            println!("Types: INT, TEXT, VECTOR[dim]");
        }
        ".tables" => {
            let tables = db.table_names();
            if tables.is_empty() {
                println!("(no tables)");
            } else {
                for t in &tables {
                    println!("  {t}");
                }
            }
        }
        cmd if cmd.starts_with(".mode ") => {
            let arg = cmd[6..].trim();
            *mode = match arg {
                "table" => OutputMode::Table,
                "csv" => OutputMode::Csv,
                "json" => OutputMode::Json,
                other => {
                    eprintln!("Unknown mode '{other}'. Use table, csv, or json.");
                    return;
                }
            };
            println!("mode set to {arg}");
        }
        cmd if cmd.starts_with(".schema ") => {
            let name = cmd[8..].trim();
            match db.table_schema(name) {
                Some(schema) => {
                    for (col, ty) in schema.columns.iter().zip(&schema.types) {
                        let ty_str = match ty {
                            tpt_archon_relational::database::ColumnType::Int => "INT",
                            tpt_archon_relational::database::ColumnType::Boolean => "BOOLEAN",
                            tpt_archon_relational::database::ColumnType::Float => "FLOAT",
                            tpt_archon_relational::database::ColumnType::Double => "DOUBLE",
                            tpt_archon_relational::database::ColumnType::Numeric => "NUMERIC",
                            tpt_archon_relational::database::ColumnType::Text => "TEXT",
                            tpt_archon_relational::database::ColumnType::Varchar(_) => "VARCHAR",
                            tpt_archon_relational::database::ColumnType::Date => "DATE",
                            tpt_archon_relational::database::ColumnType::Timestamp => "TIMESTAMP",
                            tpt_archon_relational::database::ColumnType::Vector => "VECTOR",
                        };
                        println!("  {col} {ty_str}");
                    }
                }
                None => {
                    eprintln!("Error: unknown table '{name}'");
                }
            }
        }
        #[cfg(feature = "sqlite-import")]
        cmd if cmd.starts_with(".import ") => {
            let rest = cmd[8..].trim();
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.is_empty() {
                eprintln!("Usage: .import FILE.sqlite [TABLE]");
                return;
            }
            let path = parts[0];
            let table_filter = parts.get(1).copied();
            import_sqlite(db, path, table_filter);
        }
        other => {
            eprintln!("Unknown command: {other}");
            println!("Type .help for available commands.");
        }
    }
}

#[cfg(feature = "sqlite-import")]
fn import_sqlite(db: &mut Database, path: &str, table_filter: Option<&str>) {
    use tpt_archon_relational::database::ColumnType;
    use tpt_archon_relational::parser::parse_statement as parse;

    let conn = match rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: could not open '{path}': {e}");
            return;
        }
    };

    // Discover tables.
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    let tables = match table_filter {
        Some(f) => tables.into_iter().filter(|t| t == f).collect(),
        None => tables,
    };

    if tables.is_empty() {
        eprintln!("No tables found in '{path}'.");
        return;
    }

    for table_name in &tables {
        // Get column info.
        let mut pragma = conn
            .prepare(&format!("PRAGMA table_info({table_name})"))
            .unwrap();
        let cols: Vec<(String, String)> = pragma
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let ty: String = row.get(2)?;
                Ok((name, ty))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        if cols.is_empty() {
            eprintln!("  {table_name}: no columns found, skipping");
            continue;
        }

        // Map SQLite types to Archon types.
        let col_defs: Vec<(String, ColumnType)> = cols
            .iter()
            .map(|(name, ty)| {
                let archon_ty = match ty.to_uppercase().as_str() {
                    "INTEGER" | "INT" | "BIGINT" | "SMALLINT" | "TINYINT" => ColumnType::Int,
                    "TEXT" | "VARCHAR" | "CHAR" | "CLOB" => ColumnType::Text,
                    "REAL" | "FLOAT" | "DOUBLE" | "NUMERIC" => ColumnType::Int, // approximate
                    _ => ColumnType::Text, // default to TEXT for unknown types
                };
                (name.clone(), archon_ty)
            })
            .collect();

        // Build CREATE TABLE statement.
        let col_list: String = col_defs
            .iter()
            .map(|(name, ty)| {
                let ty_str = match ty {
                    ColumnType::Int => "INT",
                    ColumnType::Text => "TEXT",
                    ColumnType::Vector => "VECTOR",
                };
                format!("{name} {ty_str}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let create_sql = format!("CREATE TABLE {table_name} ({col_list})");

        match parse(&create_sql) {
            Ok(create_stmt) => {
                // Ignore "table already exists" errors.
                let _ = db.execute(&create_stmt, &[]);
            }
            Err(e) => {
                eprintln!("  {table_name}: CREATE TABLE parse error: {}", e.0);
                continue;
            }
        }

        // Read and insert rows.
        let col_names: Vec<String> = col_defs.iter().map(|(n, _)| n.clone()).collect();
        let select_cols = col_names.join(", ");
        let mut select = conn
            .prepare(&format!("SELECT {select_cols} FROM {table_name}"))
            .unwrap();
        let mut rows_iter = select.query([]).unwrap();
        let mut imported = 0u64;

        while let Some(row) = rows_iter.next().unwrap() {
            let mut values = Vec::new();
            for (i, (_, ty)) in col_defs.iter().enumerate() {
                match ty {
                    ColumnType::Int => {
                        let v: i64 = row.get(i).unwrap_or(0);
                        values.push(format!("{v}"));
                    }
                    ColumnType::Text => {
                        let v: String = row.get(i).unwrap_or_default();
                        // Escape single quotes.
                        let escaped = v.replace('\'', "''");
                        values.push(format!("'{escaped}'"));
                    }
                    ColumnType::Vector => {
                        values.push("NULL".to_string());
                    }
                }
            }

            let cols_str = col_names.join(", ");
            let vals_str = values.join(", ");
            let insert_sql = format!("INSERT INTO {table_name} ({cols_str}) VALUES ({vals_str})");

            match parse(&insert_sql) {
                Ok(insert_stmt) => {
                    if db.execute(&insert_stmt, &[]).is_ok() {
                        imported += 1;
                    }
                }
                Err(_) => {}
            }
        }

        println!("  {table_name}: imported {imported} rows");
    }
}
