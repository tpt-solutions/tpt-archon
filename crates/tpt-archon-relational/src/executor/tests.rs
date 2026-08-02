use super::*;
use crate::parser::{parse_select, CmpOp, DateTimeField, Expr, Literal};
use crate::planner::{plan_select, TableStats};
use alloc::string::ToString;

fn users() -> Table {
    let mut t = Table::new(alloc::vec!["id".to_string(), "age".to_string()]);
    for i in 0..10 {
        t.insert(alloc::vec![Value::Int(i), Value::Int(i * 5)]);
    }
    t
}

fn run(sql: &str, table: &Table) -> ResultSet {
    let stmt = parse_select(sql).unwrap();
    let plan = plan_select(
        &stmt,
        TableStats {
            row_count: table.rows.len() as u64,
        },
    );
    execute(&plan, table).unwrap()
}

#[test]
fn select_star_returns_all() {
    let t = users();
    let r = run("SELECT * FROM users", &t);
    assert_eq!(r.rows.len(), 10);
    assert_eq!(r.columns, t.columns);
}

#[test]
fn filter_and_project() {
    let t = users();
    let r = run("SELECT id FROM users WHERE age >= 25", &t);
    assert_eq!(r.columns, alloc::vec!["id".to_string()]);
    assert_eq!(r.rows.len(), 5);
    assert_eq!(r.rows[0], alloc::vec![Value::Int(5)]);
}

#[test]
fn limit_truncates() {
    let t = users();
    let r = run("SELECT * FROM users LIMIT 3", &t);
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn unknown_column_errors() {
    let t = users();
    let stmt = parse_select("SELECT nope FROM users").unwrap();
    let plan = plan_select(&stmt, TableStats { row_count: 10 });
    assert_eq!(
        execute(&plan, &t),
        Err(ExecError::UnknownColumn("nope".to_string()))
    );
}

#[test]
fn vector_similarity_topk() {
    let embeddings = alloc::vec![
        alloc::vec![1.0, 0.0],
        alloc::vec![0.0, 1.0],
        alloc::vec![0.9, 0.1],
    ];
    let q = alloc::vec![1.0, 0.0];
    let top = vector_topk(&embeddings, &q, 2);
    assert_eq!(top[0], 0);
    assert_eq!(top[1], 2);
}

#[test]
fn evaluates_and_or_expressions() {
    let mut t = Table::new(alloc::vec![
        "id".to_string(),
        "a".to_string(),
        "b".to_string(),
    ]);
    for i in 0..10 {
        t.insert(alloc::vec![Value::Int(i), Value::Int(i), Value::Int(i * 2),]);
    }
    let r = run("SELECT * FROM t WHERE a > 3 AND b < 12", &t);
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn evaluates_in_list() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    for i in 0..5 {
        t.insert(alloc::vec![Value::Int(i), Value::Int(i * 10)]);
    }
    let r = run("SELECT * FROM t WHERE id IN (1, 3)", &t);
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn evaluates_between() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    for i in 0..10 {
        t.insert(alloc::vec![Value::Int(i), Value::Int(i)]);
    }
    let r = run("SELECT * FROM t WHERE x BETWEEN 3 AND 7", &t);
    assert_eq!(r.rows.len(), 5);
}

#[test]
fn evaluates_like() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "name".to_string()]);
    t.insert(alloc::vec![Value::Int(0), Value::Text("alice".to_string()),]);
    t.insert(alloc::vec![Value::Int(1), Value::Text("bob".to_string()),]);
    t.insert(alloc::vec![
        Value::Int(2),
        Value::Text("alicia".to_string()),
    ]);
    let r = run("SELECT * FROM t WHERE name LIKE 'al%'", &t);
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn sort_ascending() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    t.insert(alloc::vec![Value::Int(2), Value::Int(20)]);
    t.insert(alloc::vec![Value::Int(0), Value::Int(0)]);
    t.insert(alloc::vec![Value::Int(1), Value::Int(10)]);
    let r = run("SELECT * FROM t ORDER BY x ASC", &t);
    assert_eq!(r.rows[0][1], Value::Int(0));
    assert_eq!(r.rows[1][1], Value::Int(10));
    assert_eq!(r.rows[2][1], Value::Int(20));
}

#[test]
fn sort_descending() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    t.insert(alloc::vec![Value::Int(0), Value::Int(10)]);
    t.insert(alloc::vec![Value::Int(1), Value::Int(30)]);
    t.insert(alloc::vec![Value::Int(2), Value::Int(20)]);
    let r = run("SELECT * FROM t ORDER BY x DESC", &t);
    assert_eq!(r.rows[0][1], Value::Int(30));
    assert_eq!(r.rows[1][1], Value::Int(20));
    assert_eq!(r.rows[2][1], Value::Int(10));
}

#[test]
fn order_by_non_select_column_errors() {
    let t = users();
    let stmt = parse_select("SELECT id FROM users ORDER BY nonexistent ASC").unwrap();
    let plan = plan_select(&stmt, TableStats { row_count: 10 });
    assert!(matches!(
        execute(&plan, &t),
        Err(ExecError::UnknownColumn(_))
    ));
}

#[test]
fn aggregate_count() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    for i in 0..5 {
        t.insert(alloc::vec![Value::Int(i), Value::Int(i)]);
    }
    let r = run("SELECT COUNT(*) FROM t", &t);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(5));
}

#[test]
fn aggregate_sum() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    for i in 1..=4 {
        t.insert(alloc::vec![Value::Int(i), Value::Int(i)]);
    }
    let r = run("SELECT SUM(x) FROM t", &t);
    assert_eq!(r.rows[0][0], Value::Int(10));
}

#[test]
fn aggregate_min_max_over_all_null_group_is_null() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    t.insert(alloc::vec![Value::Int(0), Value::Null]);
    t.insert(alloc::vec![Value::Int(1), Value::Null]);
    let r = run("SELECT MIN(x), MAX(x) FROM t", &t);
    assert_eq!(r.rows[0][0], Value::Null);
    assert_eq!(r.rows[0][1], Value::Null);
}

#[test]
fn text_comparison_compares_content_not_length() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "name".to_string()]);
    t.insert(alloc::vec![Value::Int(0), Value::Text("bob".to_string())]);
    t.insert(alloc::vec![Value::Int(1), Value::Text("amy".to_string())]);
    let r = run("SELECT id FROM t WHERE name = 'bob'", &t);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(0));
}

#[test]
fn is_null_is_false_for_non_null_int() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    t.insert(alloc::vec![Value::Int(0), Value::Int(5)]);
    t.insert(alloc::vec![Value::Int(1), Value::Null]);
    let r = run("SELECT id FROM t WHERE x IS NULL", &t);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));
}

#[test]
fn is_not_null_negates() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string()]);
    t.insert(alloc::vec![Value::Int(0), Value::Int(5)]);
    t.insert(alloc::vec![Value::Int(1), Value::Null]);
    let r = run("SELECT id FROM t WHERE x IS NOT NULL", &t);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(0));
}

#[test]
fn not_negates_expression() {
    let mut t = Table::new(alloc::vec!["id".to_string(), "x".to_string(),]);
    for i in 0..5 {
        t.insert(alloc::vec![Value::Int(i), Value::Int(i)]);
    }
    let r = run("SELECT id FROM t WHERE NOT x > 2", &t);
    assert_eq!(r.rows.len(), 3);
}

// A1: Kleene three-valued logic — NULL AND true = NULL (falsy in WHERE).
#[test]
fn kleene_null_and_true_propagates_null() {
    let mut t = Table::new(alloc::vec![
        "id".to_string(),
        "a".to_string(),
        "b".to_string(),
    ]);
    t.insert(alloc::vec![Value::Int(0), Value::Null, Value::Int(1)]);
    t.insert(alloc::vec![Value::Int(1), Value::Int(5), Value::Int(1)]);
    // NULL AND (b = 1) → NULL → row filtered out (Kleene: NULL not true)
    let r = run("SELECT id FROM t WHERE a = 5 AND b = 1", &t);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));
}

// A1: Kleene three-valued logic — NULL OR false = NULL (falsy in WHERE).
#[test]
fn kleene_null_or_false_propagates_null() {
    let mut t = Table::new(alloc::vec![
        "id".to_string(),
        "a".to_string(),
        "b".to_string(),
    ]);
    t.insert(alloc::vec![Value::Int(0), Value::Null, Value::Int(0)]);
    t.insert(alloc::vec![Value::Int(1), Value::Int(5), Value::Int(0)]);
    // (a = 5) OR NULL → NULL → row filtered out
    let r = run("SELECT id FROM t WHERE a = 5 OR b = 1", &t);
    // id=1: a=5 true OR false = true ✓
    // id=0: NULL OR false = NULL → filtered out
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));
}

// A1: eval_scalar returns None for NULL comparison.
#[test]
fn eval_scalar_returns_none_for_null_comparison() {
    let row = alloc::vec![Value::Null];
    let cols = alloc::vec!["x".to_string()];
    let expr = Expr::Cmp {
        column: "x".to_string(),
        op: CmpOp::Eq,
        value: Literal::Null,
    };
    assert!(eval_scalar(&expr, &cols, &row).unwrap().is_none());
}

// A1: eval_scalar returns Some(Int(1)) for true comparison.
#[test]
fn eval_scalar_returns_int_for_true_comparison() {
    let row = alloc::vec![Value::Int(5)];
    let cols = alloc::vec!["x".to_string()];
    let expr = Expr::Cmp {
        column: "x".to_string(),
        op: CmpOp::Eq,
        value: Literal::Int(5),
    };
    assert_eq!(
        eval_scalar(&expr, &cols, &row).unwrap(),
        Some(Value::Int(1))
    );
}

#[test]
fn extract_cmp_matches_correct_year() {
    // Simulate a DATE column: 2024-06-15 = days since epoch ≈ 19900
    // Actually: 2024-06-15 = days since 1970-01-01
    // Let me use a known value: 2024-01-01 = 19723 days (approximately)
    // 2024 years, month 1, day 1
    let row = alloc::vec![Value::Int(19723)]; // approx 2024-01-01 as days
    let cols = alloc::vec!["created".to_string()];
    let expr = Expr::ExtractCmp {
        field: DateTimeField::Year,
        source: "created".to_string(),
        op: CmpOp::Eq,
        value: Literal::Int(2024),
    };
    assert!(eval_expr(&expr, &cols, &row).unwrap());
}

#[test]
fn extract_cmp_rejects_wrong_year() {
    let row = alloc::vec![Value::Int(19723)];
    let cols = alloc::vec!["created".to_string()];
    let expr = Expr::ExtractCmp {
        field: DateTimeField::Year,
        source: "created".to_string(),
        op: CmpOp::Eq,
        value: Literal::Int(2023),
    };
    assert!(!eval_expr(&expr, &cols, &row).unwrap());
}

#[test]
fn extract_cmp_works_with_hour_field() {
    // Timestamp: 2024-01-01 10:30:00 UTC
    // Unix timestamp for 2024-01-01 00:00:00 = 1704067200
    // 10:30:00 = 10*3600 + 30*60 = 37800 seconds offset
    // micros = (1704067200 + 37800) * 1000000 = 1704105000000000
    let row = alloc::vec![Value::Int(1_704_105_000_000_000)];
    let cols = alloc::vec!["ts".to_string()];
    let expr = Expr::ExtractCmp {
        field: DateTimeField::Hour,
        source: "ts".to_string(),
        op: CmpOp::Eq,
        value: Literal::Int(10),
    };
    assert!(eval_expr(&expr, &cols, &row).unwrap());
}

#[test]
fn extract_cmp_null_source_yields_null() {
    let row = alloc::vec![Value::Null];
    let cols = alloc::vec!["ts".to_string()];
    let expr = Expr::ExtractCmp {
        field: DateTimeField::Month,
        source: "ts".to_string(),
        op: CmpOp::Eq,
        value: Literal::Int(1),
    };
    // NULL op value → None (Kleene NULL propagation)
    assert!(!eval_expr(&expr, &cols, &row).unwrap());
}
