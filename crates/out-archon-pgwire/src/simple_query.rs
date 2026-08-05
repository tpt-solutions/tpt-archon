//! Simple Query protocol: parses `Q` messages, splits on `;`, strips comments,
//! executes each statement against a [`Database`], and emits the PostgreSQL
//! response stream. SET/SHOW/RESET are handled natively by the parser and
//! executor; no fake AST construction needed.

use alloc::string::String;

use tpt_archon_relational::{
    database::{Database, DbError},
    executor::Value,
    parser::{parse_statement, Statement},
};

use crate::codec::MessageWriter;
use crate::session::Session;

pub fn handle_simple_query(query: &str, db: &mut Database, session: &mut Session) -> Vec<u8> {
    let mut out = Vec::new();

    for raw_stmt in split_statements(query) {
        let raw_stmt = raw_stmt.trim();
        if raw_stmt.is_empty() {
            continue;
        }

        let Ok(stmt) = parse_sql(raw_stmt) else {
            let err = DbError::Unsupported(raw_stmt.to_string());
            emit_error(&mut out, &err, session);
            continue;
        };

        let Ok((rs, tag)) = db.execute_with_stats(&stmt, &[]) else {
            let err = DbError::Unsupported(raw_stmt.to_string());
            emit_error(&mut out, &err, session);
            continue;
        };

        update_txn_status(session, &stmt);

        if !rs.columns.is_empty() {
            let cols: Vec<_> = rs
                .columns
                .iter()
                .map(|name| crate::codec::message::ColumnDesc {
                    name: name.clone(),
                    table_oid: 0,
                    column_attr: 0,
                    type_oid: type_oid_for_value(&Value::Null),
                    type_size: -1,
                    type_mod: -1,
                    format: 0,
                })
                .collect();
            let mut w = MessageWriter::new();
            w.write_row_description(&cols);
            out.extend_from_slice(w.bytes());
            for row in &rs.rows {
                let mut w = MessageWriter::new();
                w.write_data_row(&cols, row);
                out.extend_from_slice(w.bytes());
            }
        }

        let mut w = MessageWriter::new();
        let tag_str = tag.as_str().to_string();
        let n = tag.row_count().unwrap_or(0);
        w.write_command_complete(&tag_str, n);
        out.extend_from_slice(w.bytes());
    }

    let mut w = MessageWriter::new();
    let txn = match session.txn_status {
        crate::session::SessionTxnStatus::Idle => crate::codec::message::TxnStatus::Idle,
        crate::session::SessionTxnStatus::InTransaction => {
            crate::codec::message::TxnStatus::InTransaction
        }
        crate::session::SessionTxnStatus::Failed => crate::codec::message::TxnStatus::Failed,
    };
    w.write_ready_for_query(txn);
    out.extend_from_slice(w.bytes());
    out
}

fn update_txn_status(session: &mut Session, stmt: &Statement) {
    session.txn_status = match session.txn_status {
        crate::session::SessionTxnStatus::InTransaction => match stmt {
            Statement::Commit | Statement::Rollback => crate::session::SessionTxnStatus::Idle,
            _ => crate::session::SessionTxnStatus::InTransaction,
        },
        crate::session::SessionTxnStatus::Failed => match stmt {
            Statement::Rollback => crate::session::SessionTxnStatus::Idle,
            _ => crate::session::SessionTxnStatus::Failed,
        },
        crate::session::SessionTxnStatus::Idle => match stmt {
            Statement::Begin => crate::session::SessionTxnStatus::InTransaction,
            _ => crate::session::SessionTxnStatus::Idle,
        },
    };
}

fn split_statements(query: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut in_dollar_quote = false;
    let mut dollar_tag = String::new();
    let mut chars = query.chars().peekable();

    while let Some(c) = chars.next() {
        if in_dollar_quote {
            current.push(c);
            if c == '$' {
                if let Some(next_c) = chars.peek() {
                    if *next_c == '$' {
                        chars.next();
                        current.push('$');
                        in_dollar_quote = false;
                        dollar_tag.clear();
                    }
                }
            }
            continue;
        }

        match c {
            '\'' if !in_string => {
                in_string = true;
                current.push(c);
            }
            '\'' if in_string => {
                in_string = false;
                current.push(c);
            }
            '$' if !in_string => {
                current.push(c);
                if let Some(next_c) = chars.peek() {
                    if *next_c == '$' {
                        chars.next();
                        current.push('$');
                        in_dollar_quote = true;
                        dollar_tag.clear();
                    }
                }
            }
            ';' if !in_string => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    stmts.push(trimmed);
                }
                current.clear();
            }
            '-' if chars.peek() == Some(&'-') && !in_string => {
                current.push(c);
                chars.next();
                for nc in chars.by_ref() {
                    if nc == '\n' {
                        current.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') && !in_string => {
                current.push(c);
                chars.next();
                while let Some(nc) = chars.next() {
                    current.push(nc);
                    if nc == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        current.push('/');
                        break;
                    }
                }
            }
            _ => current.push(c),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        stmts.push(trimmed);
    }
    stmts
}

fn strip_comments(sql: &str) -> String {
    let mut out = String::new();
    let mut in_string = false;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_string => {
                in_string = true;
                out.push(c);
            }
            '\'' if in_string => {
                in_string = false;
                out.push(c);
            }
            '-' if chars.peek() == Some(&'-') && !in_string => {
                chars.next();
                for nc in chars.by_ref() {
                    if nc == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') && !in_string => {
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn parse_sql(raw: &str) -> Result<Statement, String> {
    let cleaned = strip_comments(raw);
    parse_statement(&cleaned).map_err(|e| e.0)
}

fn emit_error(out: &mut Vec<u8>, err: &DbError, session: &mut Session) {
    let mut w = MessageWriter::new();
    let ss = crate::sqlstate::sqlstate_for_db_error(err);
    w.write_error_response(&alloc::format!("{err}"), Some(*ss.as_bytes()));
    out.extend_from_slice(w.bytes());
    session.set_failed();
}

fn type_oid_for_value(_val: &Value) -> i32 {
    match _val {
        Value::Int(_) => 20,
        Value::Float(_) => 701,
        Value::Text(_) => 25,
        Value::Vector(_) => 25, // pgvector's textual form [0.1,0.2,...] uses TEXT OID
        Value::Bool(_) => 16,   // Postgres's real `boolean` OID
        Value::Null => 25,
    }
}
