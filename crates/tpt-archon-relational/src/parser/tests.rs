use alloc::string::ToString;

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
fn parses_true_false_literals_in_where() {
    let s = parse_select("SELECT * FROM t WHERE active = TRUE").unwrap();
    assert!(matches!(
        s.filter,
        Some(Expr::Cmp { ref column, value: Literal::Bool(true), .. })
            if column == "active"
    ));
    let s = parse_select("SELECT * FROM t WHERE active = FALSE").unwrap();
    assert!(matches!(
        s.filter,
        Some(Expr::Cmp { ref column, value: Literal::Bool(false), .. })
            if column == "active"
    ));
}

#[test]
fn parses_doubled_quote_escape_in_text_literal() {
    let s = parse_select("SELECT * FROM t WHERE name = 'It''s'").unwrap();
    assert!(matches!(
        s.filter,
        Some(Expr::Cmp { ref column, value: Literal::Text(ref v), .. })
            if column == "name" && v == "It's"
    ));
}

#[test]
fn parses_from_less_select_literal() {
    let s = parse_statement("SELECT 1").unwrap();
    assert_eq!(
        s,
        Statement::SelectLiteral(alloc::vec![SelectLiteralItem {
            value: Literal::Int(1),
            alias: None,
        }])
    );
}

#[test]
fn parses_from_less_select_literal_with_alias_and_multiple_items() {
    let s = parse_statement("SELECT 1 AS one, 'x' AS letter, TRUE").unwrap();
    assert_eq!(
        s,
        Statement::SelectLiteral(alloc::vec![
            SelectLiteralItem {
                value: Literal::Int(1),
                alias: Some("one".to_string()),
            },
            SelectLiteralItem {
                value: Literal::Text("x".to_string()),
                alias: Some("letter".to_string()),
            },
            SelectLiteralItem {
                value: Literal::Bool(true),
                alias: None,
            },
        ])
    );
}

#[test]
fn select_literal_does_not_swallow_real_from_select() {
    // A leading literal followed by FROM must fall through to the normal
    // column/table parser (which still rejects a bare literal as a column
    // name — scalar expressions in a real table's SELECT list are a
    // separate, still-unsupported gap), not be misparsed as a from-less
    // literal select.
    let s = parse_statement("SELECT 1 FROM t");
    assert!(s.is_err());
}

#[test]
fn bare_column_select_without_from_is_still_a_parse_error() {
    // Only a literal-only projection list qualifies for the FROM-less path;
    // a bare column reference still requires FROM (there's no table to
    // resolve it against).
    let s = parse_statement("SELECT v");
    assert!(s.is_err());
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
    assert_eq!(s.joins[0].jtype, JoinType::Inner);
    assert!(s.joins[0].on_expr.is_some());
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
        assert_eq!(i.values[0][1], Literal::Null);
    } else {
        panic!("expected Insert");
    }
}

#[test]
fn parses_multi_row_insert() {
    let s = parse_statement("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    if let Statement::Insert(i) = s {
        assert_eq!(i.values.len(), 3);
        assert_eq!(i.values[0], alloc::vec![Literal::Int(1), Literal::Int(10)]);
        assert_eq!(i.values[1], alloc::vec![Literal::Int(2), Literal::Int(20)]);
        assert_eq!(i.values[2], alloc::vec![Literal::Int(3), Literal::Int(30)]);
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
    let s =
        parse_statement("SELECT * FROM (SELECT id, name FROM t WHERE age >= 20) AS sub").unwrap();
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
    let s =
        parse_statement("WITH cte AS (SELECT id FROM t WHERE age > 18) SELECT * FROM cte").unwrap();
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
    let s =
        parse_statement("WITH a AS (SELECT id FROM t), b AS (SELECT id FROM t) SELECT * FROM a")
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
    let r = parse_statement("WITH RECURSIVE cte AS (SELECT 1) SELECT * FROM cte");
    assert!(r.is_err());
}

#[test]
fn parses_having() {
    let s = parse_select("SELECT dept, COUNT(*) FROM t GROUP BY dept HAVING COUNT(*) > 1").unwrap();
    assert!(s.having.is_some());
    assert_eq!(s.group_by, alloc::vec!["dept".to_string()]);
}

// Regression test: ALTER TABLE ADD COLUMN v VECTOR with no [N] must not
// corrupt the token stream (A0 bug fix).
#[test]
fn alter_table_vector_without_dimension_parses() {
    let s = parse_statement("ALTER TABLE t ADD COLUMN v VECTOR").unwrap();
    match s {
        Statement::AlterTable(at) => match at.op {
            AlterTableOp::AddColumn(ref cd) => {
                assert_eq!(cd.name, "v");
                assert_eq!(cd.ctype, ColumnType::Vector);
            }
            _ => panic!("expected AddColumn"),
        },
        _ => panic!("expected AlterTable"),
    }
}

// Token stream supports multi-token lookahead via peek().
#[test]
fn token_stream_peek_does_not_consume() {
    let s = parse_select("SELECT id, name FROM t WHERE age >= 18").unwrap();
    assert_eq!(s.columns, alloc::vec!["id".to_string(), "name".to_string()]);
}

// Regression test: unbounded recursive-descent parsing (security audit
// finding 3): pathological nesting must return a ParseError, not
// blow the call stack.
#[test]
fn deeply_nested_parens_in_where_returns_parse_error_not_stack_overflow() {
    let depth = 50_000;
    let mut sql = "SELECT * FROM t WHERE ".to_string();
    for _ in 0..depth {
        sql.push('(');
    }
    sql.push_str("a = 1");
    for _ in 0..depth {
        sql.push(')');
    }
    let r = parse_statement(&sql);
    assert!(r.is_err(), "50,000 nested parens must be rejected");
}

#[test]
fn long_not_chain_returns_parse_error_not_stack_overflow() {
    let depth = 50_000;
    let mut sql = "SELECT * FROM t WHERE ".to_string();
    for _ in 0..depth {
        sql.push_str("NOT ");
    }
    sql.push_str("a = 1");
    let r = parse_statement(&sql);
    assert!(r.is_err(), "50,000 chained NOTs must be rejected");
}

#[test]
fn parses_extract_cmp_in_where() {
    let s = parse_select("SELECT * FROM t WHERE EXTRACT(YEAR FROM created) = 2024").unwrap();
    assert_eq!(
        s.filter,
        Some(Expr::ExtractCmp {
            field: DateTimeField::Year,
            source: "created".to_string(),
            op: CmpOp::Eq,
            value: Literal::Int(2024),
        })
    );
}

#[test]
fn parses_extract_is_null() {
    let s = parse_select("SELECT * FROM t WHERE EXTRACT(month FROM ts) IS NULL").unwrap();
    assert_eq!(
        s.filter,
        Some(Expr::IsNull {
            column: "ts".to_string(),
            negated: false,
        })
    );
}

#[test]
fn parses_extract_is_not_null() {
    let s = parse_select("SELECT * FROM t WHERE EXTRACT(day FROM started) IS NOT NULL").unwrap();
    assert_eq!(
        s.filter,
        Some(Expr::IsNull {
            column: "started".to_string(),
            negated: true,
        })
    );
}

#[test]
fn rejects_extract_without_operator() {
    let r = parse_statement("SELECT * FROM t WHERE EXTRACT(YEAR FROM col)");
    assert!(r.is_err(), "EXTRACT without operator must be rejected");
}

#[test]
fn extract_requires_source_column() {
    let r = parse_statement("SELECT * FROM t WHERE EXTRACT(YEAR FROM 42) = 1");
    assert!(r.is_err(), "EXTRACT requires a column name");
}

#[test]
fn deeply_nested_exists_subquery_returns_parse_error_not_stack_overflow() {
    let depth = 5_000;
    let mut sql = "SELECT * FROM t WHERE ".to_string();
    for _ in 0..depth {
        sql.push_str("EXISTS (SELECT * FROM t2 WHERE ");
    }
    sql.push_str("a = 1");
    for _ in 0..depth {
        sql.push(')');
    }
    let r = parse_statement(&sql);
    assert!(
        r.is_err(),
        "5,000 nested EXISTS subqueries must be rejected"
    );
}

#[test]
fn deeply_nested_from_subquery_returns_parse_error_not_stack_overflow() {
    let depth = 5_000;
    let mut sql = "SELECT * FROM ".to_string();
    for _ in 0..depth {
        sql.push_str("(SELECT * FROM ");
    }
    sql.push('t');
    for i in 0..depth {
        sql.push_str(") AS x");
        sql.push_str(&i.to_string());
    }
    let r = parse_statement(&sql);
    assert!(r.is_err(), "5,000 nested FROM-subqueries must be rejected");
}

#[test]
fn moderate_nesting_still_parses_successfully() {
    let depth = 50;
    let mut sql = "SELECT * FROM t WHERE ".to_string();
    for _ in 0..depth {
        sql.push('(');
    }
    sql.push_str("a = 1");
    for _ in 0..depth {
        sql.push(')');
    }
    let r = parse_statement(&sql);
    assert!(r.is_ok(), "moderate nesting should still parse: {r:?}");
}
