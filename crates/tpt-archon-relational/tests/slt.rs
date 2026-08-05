//! Hand-rolled `.slt` (sqllogictest-format) corpus runner.
//!
//! This is `tpt-archon-relational`'s first `tests/` integration directory —
//! previously the crate only had inline `#[cfg(test)]` modules in `src/*.rs`.
//! It runs every corpus file under `tests/slt/supported/` and
//! `tests/slt/divergent/` (see `crates/tpt-archon-relational/tests/slt/`)
//! against `Database` directly through the same public entry point the
//! `archon-sql` REPL uses: `parser::parse_statement` -> `Database::execute`.
//!
//! Deliberately zero new dependencies: this crate's only external dependency
//! today is one optional (`tpt-gpu-ir-spec`, feature-gated), and pulling in
//! the real `sqllogictest` crate would add a ~40-crate tree (`async-trait`,
//! `futures`, `regex`, tokio-adjacent bits) just to run a test. This module
//! hand-rolls the small directive subset actually needed. Staying file-format
//! compatible with real `.slt` means `sqllogictest-bin` (or the
//! `out-archon-pgcompat` oracle crate; see its own doc comment) can still
//! point at the exact same corpus files.
//!
//! ## Corpus format (directives this runner understands)
//!
//! - `# comment` — a full-line comment; ignored wherever it appears.
//! - Blank lines separate directives from each other and from their bodies.
//! - `statement ok` followed by one SQL statement (which may span multiple
//!   lines, ending at the next blank line): the statement must both parse
//!   and execute successfully.
//! - `statement error <substring>` followed by one SQL statement: EITHER
//!   parsing or execution must fail, and `<substring>` must appear in the
//!   `Debug`-formatted error (substring match, not regex). Note `ParseError`'s
//!   `Debug` output is a human-readable message (e.g.
//!   `ParseError("expected FROM")`), but `DbError` is a plain
//!   `#[derive(Debug)]` enum — most variants carry no message, just their
//!   PascalCase variant name plus whatever identifier triggered the error
//!   (e.g. `TableAlreadyExists("widgets")`), so patterns for `DbError` cases
//!   are variant names, not English prose. See `tests/slt/supported/ddl.slt`
//!   for the fuller version of this note with worked examples.
//! - `query <types>` followed by one SELECT statement, then a line
//!   containing exactly `----`, then the expected result rows: the
//!   statement must parse and execute successfully, and its rows must match.
//!   `<types>` is one letter per output column (`I` = integer, `T` = text),
//!   used only as a column-count sanity check, not to change comparison
//!   behavior (`Value` has no float/bool variant today, so no float-specific
//!   formatting rule is needed).
//!
//! ## Value formatting (must match how corpus files write expected rows)
//!
//! - SQL `NULL` renders as the literal token `NULL`.
//! - Integers render as plain decimal (`Value::Int`'s `Display`/`to_string`).
//! - Text renders as-is, unquoted — so corpus text values must not contain
//!   whitespace (each row is one line; column values within a row are
//!   space-separated).
//!
//! ## Row-order rule
//!
//! Comparison is order-INSENSITIVE (both expected and actual row sets are
//! sorted before comparing) *unless* the query text contains `ORDER BY`
//! (case-insensitive substring check), in which case rows must match in the
//! exact order returned.
//!
//! Each `.slt` file gets its own fresh `Database::empty()` — directives
//! within one file share state (so a file can `CREATE TABLE` then `INSERT`
//! then `SELECT`), but no state leaks between files.

use std::fs;
use std::path::{Path, PathBuf};

use tpt_archon_relational::database::Database;
use tpt_archon_relational::executor::Value;
use tpt_archon_relational::parser::parse_statement;

/// One parsed `.slt` directive, plus the 1-indexed source line it started on
/// (for panic messages).
enum Directive {
    StatementOk {
        sql: String,
        line: usize,
    },
    StatementError {
        sql: String,
        pattern: String,
        line: usize,
    },
    Query {
        sql: String,
        types: String,
        expected: Vec<Vec<String>>,
        line: usize,
    },
}

/// Parses the directive subset described in this file's module doc comment
/// out of raw `.slt` text.
fn parse_directives(text: &str) -> Vec<Directive> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;

    // Collects lines starting at `*i` up to (not including) the next blank
    // line or EOF, skipping `#`-comment lines, joining the rest with spaces.
    // Advances `*i` past the consumed lines (but not past the blank
    // terminator, which the caller's outer loop will skip on its own).
    fn collect_block(lines: &[&str], i: &mut usize, stop_at_dashes: bool) -> String {
        let mut parts: Vec<&str> = Vec::new();
        while *i < lines.len() {
            let line = lines[*i];
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if stop_at_dashes && trimmed == "----" {
                break;
            }
            if !trimmed.starts_with('#') {
                parts.push(trimmed);
            }
            *i += 1;
        }
        parts.join(" ")
    }

    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        if trimmed == "statement ok" {
            let line = i + 1;
            i += 1;
            let sql = collect_block(&lines, &mut i, false);
            assert!(
                !sql.is_empty(),
                "line {line}: `statement ok` with no SQL body"
            );
            out.push(Directive::StatementOk { sql, line });
            continue;
        }

        if let Some(pattern) = trimmed.strip_prefix("statement error ") {
            let pattern = pattern.trim().to_string();
            let line = i + 1;
            i += 1;
            let sql = collect_block(&lines, &mut i, false);
            assert!(
                !sql.is_empty(),
                "line {line}: `statement error` with no SQL body"
            );
            out.push(Directive::StatementError { sql, pattern, line });
            continue;
        }

        if let Some(types) = trimmed.strip_prefix("query ") {
            let types = types.trim().to_string();
            let line = i + 1;
            i += 1;
            let sql = collect_block(&lines, &mut i, true);
            assert!(!sql.is_empty(), "line {line}: `query` with no SQL body");

            // Skip blank lines between the SQL body and the `----` marker.
            while i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
            assert_eq!(
                lines.get(i).map(|l| l.trim()),
                Some("----"),
                "line {line}: expected `----` after query SQL"
            );
            i += 1;

            let mut expected = Vec::new();
            while i < lines.len() {
                let row_line = lines[i];
                let row_trimmed = row_line.trim();
                if row_trimmed.is_empty() {
                    break;
                }
                if row_trimmed.starts_with('#') {
                    i += 1;
                    continue;
                }
                expected.push(
                    row_trimmed
                        .split_whitespace()
                        .map(|s| s.to_string())
                        .collect::<Vec<String>>(),
                );
                i += 1;
            }

            out.push(Directive::Query {
                sql,
                types,
                expected,
                line,
            });
            continue;
        }

        panic!("line {}: unrecognized directive: {trimmed:?}", i + 1);
    }

    out
}

/// Formats a [`Value`] the same way corpus files write expected query
/// output — see this file's module doc comment for the exact rules.
fn display_value(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        // Matches Postgres's text-format boolean rendering ("t"/"f", not
        // "true"/"false") so this runner's expected-row text stays
        // byte-comparable with a real Postgres oracle (see out-archon-pgcompat).
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Null => "NULL".to_string(),
        Value::Vector(v) => {
            // Not exercised by this corpus (see supported/ddl.slt's format
            // note on why vector coverage is out of scope for slice 1), but
            // handled rather than panicking so a future corpus addition
            // fails on a row mismatch instead of an unrelated panic here.
            format!(
                "[{}]",
                v.iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

/// Runs every directive in one `.slt` file against a fresh `Database`.
fn run_file(path: &Path) {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let directives = parse_directives(&text);
    let mut db = Database::empty();

    for directive in directives {
        match directive {
            Directive::StatementOk { sql, line } => {
                let stmt = parse_statement(&sql).unwrap_or_else(|e| {
                    panic!(
                        "{}:{line}: `statement ok` failed to PARSE: {e:?}\n  sql: {sql}",
                        path.display()
                    )
                });
                db.execute(&stmt, &[]).unwrap_or_else(|e| {
                    panic!(
                        "{}:{line}: `statement ok` failed to EXECUTE: {e:?}\n  sql: {sql}",
                        path.display()
                    )
                });
            }
            Directive::StatementError { sql, pattern, line } => {
                match parse_statement(&sql) {
                    Err(e) => {
                        let dbg = format!("{e:?}");
                        assert!(
                            dbg.contains(&pattern),
                            "{}:{line}: expected parse error containing {pattern:?}, got {dbg:?}\n  sql: {sql}",
                            path.display()
                        );
                    }
                    Ok(stmt) => match db.execute(&stmt, &[]) {
                        Err(e) => {
                            let dbg = format!("{e:?}");
                            assert!(
                                dbg.contains(&pattern),
                                "{}:{line}: expected execution error containing {pattern:?}, got {dbg:?}\n  sql: {sql}",
                                path.display()
                            );
                        }
                        Ok(_) => panic!(
                            "{}:{line}: expected an error containing {pattern:?}, but the statement succeeded\n  sql: {sql}",
                            path.display()
                        ),
                    },
                }
            }
            Directive::Query {
                sql,
                types,
                expected,
                line,
            } => {
                let stmt = parse_statement(&sql).unwrap_or_else(|e| {
                    panic!(
                        "{}:{line}: `query` failed to PARSE: {e:?}\n  sql: {sql}",
                        path.display()
                    )
                });
                let rs = db.execute(&stmt, &[]).unwrap_or_else(|e| {
                    panic!(
                        "{}:{line}: `query` failed to EXECUTE: {e:?}\n  sql: {sql}",
                        path.display()
                    )
                });
                assert_eq!(
                    rs.columns.len(),
                    types.len(),
                    "{}:{line}: column count mismatch: type string {types:?} implies {} \
                     column(s), got {} ({:?})\n  sql: {sql}",
                    path.display(),
                    types.len(),
                    rs.columns.len(),
                    rs.columns,
                );

                let mut actual: Vec<Vec<String>> = rs
                    .rows
                    .iter()
                    .map(|row| row.iter().map(display_value).collect())
                    .collect();
                let mut expected = expected;

                let ordered = sql.to_ascii_uppercase().contains("ORDER BY");
                if !ordered {
                    actual.sort();
                    expected.sort();
                }

                assert_eq!(
                    actual,
                    expected,
                    "{}:{line}: row mismatch (order-sensitive: {ordered})\n  sql: {sql}",
                    path.display()
                );
            }
        }
    }
}

/// Recursively collects `.slt` file paths under `dir`, sorted for
/// deterministic test order.
fn collect_slt_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read directory {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_slt_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("slt") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Runs the full `.slt` corpus (`supported/` and `divergent/`) against
/// `Database` directly. `divergent/` files encode their own expected
/// (diverging) behavior via `statement error`/`query` directives, so running
/// them here just asserts Archon keeps behaving the way it's documented to
/// — it does not compare against real Postgres (that's
/// `crates/out-archon-pgcompat`'s job, run separately and only with a live
/// Postgres available).
#[test]
fn slt_corpus() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let mut files = collect_slt_files(&root.join("supported"));
    files.extend(collect_slt_files(&root.join("divergent")));
    assert!(
        !files.is_empty(),
        "no .slt files found under {}",
        root.display()
    );

    for path in files {
        run_file(&path);
    }
}
