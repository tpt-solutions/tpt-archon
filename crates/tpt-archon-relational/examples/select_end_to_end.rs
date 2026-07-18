//! An end-to-end `SELECT`: parse SQL, plan it, and run it over an in-memory
//! table with the vectorized executor.
//!
//! Run with: `cargo run -p tpt-archon-relational --example select_end_to_end`

use tpt_archon_relational::executor::{execute, Table, Value};
use tpt_archon_relational::parser::parse_select;
use tpt_archon_relational::planner::{plan_select, TableStats};

fn main() {
    // Build a small "users" table: (id, age).
    let mut users = Table::new(vec!["id".to_string(), "age".to_string()]);
    for i in 0..20i64 {
        users.insert(vec![Value::Int(i), Value::Int(i * 3)]);
    }

    let sql = "SELECT id FROM users WHERE age >= 30 LIMIT 3";
    println!("SQL: {sql}");

    // Parse -> plan -> execute.
    let stmt = parse_select(sql).expect("parse");
    let plan = plan_select(
        &stmt,
        TableStats {
            row_count: users.rows.len() as u64,
        },
    );
    println!("plan estimated rows: {}", plan.estimated_rows);
    println!("dispatch: {:?}", plan.dispatch);

    let result = execute(&plan, &users).expect("execute");
    println!("columns: {:?}", result.columns);
    for row in &result.rows {
        println!("  {row:?}");
    }

    // age >= 30 => id >= 10; LIMIT 3 => ids 10, 11, 12.
    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0], vec![Value::Int(10)]);
    println!("end-to-end SELECT complete.");
}
