use alloc::string::ToString;

use super::*;
use crate::executor::Value;
use crate::parser::{parse_statement, InsertStatement, Statement};
use crate::vector_index;

fn schema() -> Schema {
    Schema {
        columns: alloc::vec!["id".to_string(), "name".to_string(), "age".to_string()],
        types: alloc::vec![ColumnType::Int, ColumnType::Text, ColumnType::Int],
    }
}

fn db() -> Database {
    Database::new(schema())
}

#[test]
fn execute_dispatch_insert_select_update_delete() {
    let mut d = db();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
        &[],
    )
    .unwrap();
    assert_eq!(d.len(), 1);

    let r = d
        .execute(
            &parse_statement("SELECT id, name FROM t WHERE age >= 30").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));

    d.execute(
        &parse_statement("UPDATE t SET age = 99 WHERE age < 50").unwrap(),
        &[],
    )
    .unwrap();
    let r2 = d
        .execute(
            &parse_statement("SELECT id FROM t WHERE age = 99").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r2.rows.len(), 1);

    d.execute(
        &parse_statement("DELETE FROM t WHERE age = 99").unwrap(),
        &[],
    )
    .unwrap();
    assert_eq!(d.len(), 0);
}

#[test]
fn arity_and_type_errors() {
    let mut d = db();
    let ins = InsertStatement {
        table: "t".to_string(),
        columns: alloc::vec!["id".to_string()],
        values: alloc::vec![
            crate::parser::Literal::Int(1),
            crate::parser::Literal::Int(2),
        ],
    };
    assert!(matches!(
        d.execute_checked(&Statement::Insert(ins)),
        Err(DbError::ArityMismatch)
    ));

    let bad_ty = parse_statement("INSERT INTO t (id, name, age) VALUES (1, 5, 30)").unwrap();
    assert_eq!(
        d.execute(&bad_ty, &[]),
        Err(DbError::ColumnTypeMismatch("name".to_string()))
    );
}

#[test]
fn vector_topk_query() {
    let schema = Schema {
        columns: alloc::vec!["id".to_string(), "emb".to_string()],
        types: alloc::vec![ColumnType::Int, ColumnType::Vector],
    };
    let mut d = Database::new(schema);
    let rows = ["[1.0, 0.0]", "[0.0, 1.0]", "[0.9, 0.1]"];
    for (i, emb) in rows.iter().enumerate() {
        let sql = alloc::format!("INSERT INTO t (id, emb) VALUES ({i}, {emb})");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let sel = parse_statement("SELECT id FROM t ORDER BY cosine(emb, ?) LIMIT 2").unwrap();
    let r = d.execute(&sel, &[alloc::vec![1.0, 0.0]]).unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], Value::Int(0));
    assert_eq!(r.rows[1][0], Value::Int(2));
}

#[test]
fn vector_topk_with_where_filter() {
    let schema = Schema {
        columns: alloc::vec!["id".to_string(), "emb".to_string(), "tag".to_string(),],
        types: alloc::vec![ColumnType::Int, ColumnType::Vector, ColumnType::Text],
    };
    let mut d = Database::new(schema);
    // id=0: tag=a, closest to [1,0]
    // id=1: tag=b, closest to [0,1]
    // id=2: tag=a, second closest to [1,0]
    // id=3: tag=b, closest to [1,0] but filtered out by WHERE tag='a'
    let data = &[
        (0, "[1.0, 0.0]", "a"),
        (1, "[0.0, 1.0]", "b"),
        (2, "[0.9, 0.1]", "a"),
        (3, "[0.95, 0.05]", "b"),
    ];
    for (id, emb, tag) in data {
        let sql = alloc::format!("INSERT INTO t (id, emb, tag) VALUES ({id}, {emb}, '{tag}')");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // Without WHERE: top-2 by cosine to [1,0] would be id=0, id=3.
    let sel = parse_statement("SELECT id FROM t WHERE tag = 'a' ORDER BY cosine(emb, ?) LIMIT 2")
        .unwrap();
    let r = d.execute(&sel, &[alloc::vec![1.0, 0.0]]).unwrap();
    assert_eq!(r.rows.len(), 2);
    // tag='a' rows: id=0 ([1,0]), id=2 ([0.9,0.1]) → both kept
    assert_eq!(r.rows[0][0], Value::Int(0));
    assert_eq!(r.rows[1][0], Value::Int(2));
}

#[test]
fn vector_topk_uses_ivfflat_index_past_threshold() {
    // One past `vector_index::MIN_ROWS_FOR_INDEX` so the lazy build
    // triggers on the row that crosses it, exercising the index path in
    // `run_vector_topk` instead of the brute-force scan.
    let n = vector_index::MIN_ROWS_FOR_INDEX + 1;
    let schema = Schema {
        columns: alloc::vec!["id".to_string(), "emb".to_string()],
        types: alloc::vec![ColumnType::Int, ColumnType::Vector],
    };
    let mut d = Database::new(schema);
    for i in 0..n {
        // Unique one-hot embeddings (dim == n) so nearest-neighbor
        // results are unambiguous regardless of cluster assignment.
        let mut emb = alloc::vec!["0.0".to_string(); n];
        emb[i] = "1.0".to_string();
        let sql = alloc::format!("INSERT INTO t (id, emb) VALUES ({i}, [{}])", emb.join(", "));
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    assert!(
        d.table("t")
            .unwrap()
            .vector_indexes
            .iter()
            .any(|(c, _)| c == "emb"),
        "index should have been built once the table crossed MIN_ROWS_FOR_INDEX"
    );
    let mut query = alloc::vec![0.0f32; n];
    query[5] = 1.0;
    let sel = parse_statement("SELECT id FROM t ORDER BY cosine(emb, ?) LIMIT 1").unwrap();
    let r = d.execute(&sel, &[query]).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(5));
}

#[test]
fn vector_index_maintained_on_update_and_delete() {
    let n = vector_index::MIN_ROWS_FOR_INDEX + 1;
    // dim = n + 1: one spare slot no initial row occupies, so moving a
    // row onto it via UPDATE can't tie with an existing row's vector.
    let dim = n + 1;
    let schema = Schema {
        columns: alloc::vec!["id".to_string(), "emb".to_string()],
        types: alloc::vec![ColumnType::Int, ColumnType::Vector],
    };
    let mut d = Database::new(schema);
    for i in 0..n {
        let mut emb = alloc::vec!["0.0".to_string(); dim];
        emb[i] = "1.0".to_string();
        let sql = alloc::format!("INSERT INTO t (id, emb) VALUES ({i}, [{}])", emb.join(", "));
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // Delete row 5, then query for its old embedding: it must be gone.
    d.execute(&parse_statement("DELETE FROM t WHERE id = 5").unwrap(), &[])
        .unwrap();
    let mut old_query = alloc::vec![0.0f32; dim];
    old_query[5] = 1.0;
    let sel = parse_statement("SELECT id FROM t ORDER BY cosine(emb, ?) LIMIT 3").unwrap();
    let r = d.execute(&sel, &[old_query]).unwrap();
    assert!(r.rows.iter().all(|row| row[0] != Value::Int(5)));

    // Update row 6's embedding onto the spare slot, then confirm the
    // index itself was updated (not just the tree). Checked directly
    // against the index with an exhaustive nprobe rather than through
    // SQL: the SQL path uses `vector_index::DEFAULT_NPROBE`, and a
    // one-hot spare dimension with zero training signal makes every
    // centroid tie at dot-product 0 against it, which is a pathological
    // case for *approximate* recall, not a maintenance bug — this
    // assertion is about whether `id, vector` moved to where it should
    // be inside the index, not about IVF recall under adversarial input.
    let mut new_emb = alloc::vec!["0.0".to_string(); dim];
    new_emb[n] = "1.0".to_string();
    let update_sql = alloc::format!("UPDATE t SET emb = [{}] WHERE id = 6", new_emb.join(", "));
    d.execute(&parse_statement(&update_sql).unwrap(), &[])
        .unwrap();
    let mut moved_query = alloc::vec![0.0f32; dim];
    moved_query[n] = 1.0;
    let ts = d.table("t").unwrap();
    let (_, idx) = ts
        .vector_indexes
        .iter()
        .find(|(c, _)| c == "emb")
        .expect("index should still exist after the update");
    let top = idx.search(&moved_query, 1, usize::MAX);
    assert_eq!(top[0], 6);
}

#[test]
fn create_table_and_insert() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE users (name TEXT, age INT)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("INSERT INTO users (name, age) VALUES ('alice', 30)").unwrap(),
        &[],
    )
    .unwrap();
    let r = d
        .execute(
            &parse_statement("SELECT * FROM users WHERE age >= 30").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn create_table_duplicate_errors() {
    let mut d = Database::new(schema());
    assert!(matches!(
        d.execute(&parse_statement("CREATE TABLE t (x INT)").unwrap(), &[]),
        Err(DbError::TableAlreadyExists(_))
    ));
}

#[test]
fn unknown_table_errors() {
    let mut d = Database::empty();
    assert!(matches!(
        d.execute(&parse_statement("SELECT * FROM nope").unwrap(), &[]),
        Err(DbError::UnknownTable(_))
    ));
}

#[test]
fn begin_commit_rollback() {
    let mut d = db();
    d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
    assert!(matches!(
        d.execute(&parse_statement("BEGIN").unwrap(), &[]),
        Err(DbError::TransactionError(_))
    ));
    d.execute(&parse_statement("COMMIT").unwrap(), &[]).unwrap();
    d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
    d.execute(&parse_statement("ROLLBACK").unwrap(), &[])
        .unwrap();
}

#[test]
fn rollback_actually_undoes_writes() {
    // Regression test: ROLLBACK used to be a bare no-op (just flipped
    // in_transaction back to false) — writes made inside the transaction
    // were never undone. Now they must be, via the real mvcc store.
    let mut d = db();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'seed', 1)").unwrap(),
        &[],
    )
    .unwrap();

    d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'ghost', 2)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("UPDATE t SET age = 99 WHERE id = 0").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(&parse_statement("ROLLBACK").unwrap(), &[])
        .unwrap();

    let r = d
        .execute(&parse_statement("SELECT id, age FROM t").unwrap(), &[])
        .unwrap();
    // Only the seed row should remain, with its original age.
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(0));
    assert_eq!(r.rows[0][1], Value::Int(1));
}

#[test]
fn commit_applies_writes_made_during_transaction() {
    let mut d = db();
    d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'alice', 30)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(&parse_statement("COMMIT").unwrap(), &[]).unwrap();

    let r = d
        .execute(&parse_statement("SELECT id FROM t").unwrap(), &[])
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(0));
}

#[test]
fn reads_within_transaction_see_own_writes() {
    let mut d = db();
    d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'alice', 30)").unwrap(),
        &[],
    )
    .unwrap();
    // Not yet committed, but should be visible within the same transaction.
    let r = d
        .execute(&parse_statement("SELECT id FROM t").unwrap(), &[])
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    d.execute(&parse_statement("ROLLBACK").unwrap(), &[])
        .unwrap();
}

#[test]
fn delete_within_transaction_rolls_back() {
    let mut d = db();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'alice', 30)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
    d.execute(&parse_statement("DELETE FROM t WHERE id = 0").unwrap(), &[])
        .unwrap();
    let mid = d
        .execute(&parse_statement("SELECT id FROM t").unwrap(), &[])
        .unwrap();
    assert_eq!(mid.rows.len(), 0);
    d.execute(&parse_statement("ROLLBACK").unwrap(), &[])
        .unwrap();
    let after = d
        .execute(&parse_statement("SELECT id FROM t").unwrap(), &[])
        .unwrap();
    assert_eq!(after.rows.len(), 1);
}

#[test]
fn and_or_where_filter() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement("SELECT * FROM t WHERE age > 5 AND age < 35").unwrap(),
            &[],
        )
        .unwrap();
    // ages: 0, 10, 20, 30, 40; >5 and <35 → 10, 20, 30
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn in_predicate() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!("INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {i})");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement("SELECT * FROM t WHERE age IN (1, 3)").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn between_predicate() {
    let mut d = db();
    for i in 0..10 {
        let sql = alloc::format!("INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {i})");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement("SELECT * FROM t WHERE age BETWEEN 3 AND 7").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 5);
}

#[test]
fn order_by_column() {
    let mut d = db();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'c', 30)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'a', 10)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (2, 'b', 20)").unwrap(),
        &[],
    )
    .unwrap();
    let r = d
        .execute(
            &parse_statement("SELECT * FROM t ORDER BY age ASC").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows[0][2], Value::Int(10));
    assert_eq!(r.rows[1][2], Value::Int(20));
    assert_eq!(r.rows[2][2], Value::Int(30));
}

#[test]
fn group_by_with_count() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE t (dept TEXT, salary INT)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (dept, salary) VALUES ('eng', 100)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (dept, salary) VALUES ('eng', 200)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (dept, salary) VALUES ('sales', 150)").unwrap(),
        &[],
    )
    .unwrap();
    let r = d
        .execute(
            &parse_statement("SELECT dept, COUNT(*) FROM t GROUP BY dept").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    // Each group should have count 2 and 1.
    let counts: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match &row[1] {
            Value::Int(v) => *v,
            _ => panic!("expected int"),
        })
        .collect();
    assert!(counts.contains(&2));
    assert!(counts.contains(&1));
}

#[test]
fn create_view_select_and_filter_through() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    d.execute(
        &parse_statement("CREATE VIEW adults AS SELECT * FROM t WHERE age >= 20").unwrap(),
        &[],
    )
    .unwrap();
    let r = d
        .execute(&parse_statement("SELECT id FROM adults").unwrap(), &[])
        .unwrap();
    // ages 0,10,20,30,40 -> >=20 keeps ids 2,3,4
    assert_eq!(r.rows.len(), 3);

    let r2 = d
        .execute(
            &parse_statement("SELECT id FROM adults WHERE id = 3").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r2.rows.len(), 1);
    assert_eq!(r2.rows[0][0], Value::Int(3));
}

#[test]
fn drop_view_then_select_errors() {
    let mut d = db();
    d.execute(
        &parse_statement("CREATE VIEW everyone AS SELECT * FROM t").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(&parse_statement("DROP VIEW everyone").unwrap(), &[])
        .unwrap();
    assert!(matches!(
        d.execute(&parse_statement("SELECT * FROM everyone").unwrap(), &[]),
        Err(DbError::UnknownTable(_))
    ));
}

#[test]
fn create_view_self_reference_errors() {
    let mut d = db();
    assert!(matches!(
        d.execute(
            &parse_statement("CREATE VIEW loop AS SELECT * FROM loop").unwrap(),
            &[],
        ),
        Err(DbError::RecursiveView(_))
    ));
}

#[test]
fn create_view_duplicate_name_errors() {
    let mut d = db();
    d.execute(
        &parse_statement("CREATE VIEW everyone AS SELECT * FROM t").unwrap(),
        &[],
    )
    .unwrap();
    assert!(matches!(
        d.execute(
            &parse_statement("CREATE VIEW everyone AS SELECT * FROM t").unwrap(),
            &[],
        ),
        Err(DbError::ViewAlreadyExists(_))
    ));
    assert!(matches!(
        d.execute(
            &parse_statement("CREATE VIEW t AS SELECT * FROM t").unwrap(),
            &[]
        ),
        Err(DbError::ViewAlreadyExists(_))
    ));
}

#[test]
fn subquery_in_from() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement("SELECT * FROM (SELECT id, name FROM t WHERE age >= 20) AS sub")
                .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(
        r.columns,
        alloc::vec!["sub.id".to_string(), "sub.name".to_string()]
    );
}

#[test]
fn subquery_with_alias_in_outer_where() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement(
                "SELECT sub.id FROM (SELECT id, name, age FROM t WHERE age >= 10) AS sub WHERE sub.age < 30",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    // ages 10, 20 -> ids 1, 2
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn nested_subquery() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement(
                "SELECT * FROM (SELECT * FROM (SELECT id, age FROM t WHERE age > 0) AS inner1) AS outer1",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    // ages 10, 20, 30, 40 -> 4 rows
    assert_eq!(r.rows.len(), 4);
}

#[test]
fn cte_basic() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement(
                "WITH cte AS (SELECT id, name FROM t WHERE age >= 20) SELECT * FROM cte",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn cte_multiple() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // Use a CTE to filter, then query it.
    let r = d
        .execute(
            &parse_statement(
                "WITH young AS (SELECT id, age FROM t WHERE age < 30) \
                 SELECT * FROM young WHERE age >= 10",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    // young has ages 0,10,20; WHERE age >= 10 keeps ages 10,20 = 2 rows.
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn cte_recursive_self_reference_errors() {
    let mut d = db();
    assert!(matches!(
        d.execute(
            &parse_statement("WITH cte AS (SELECT id FROM cte) SELECT * FROM cte").unwrap(),
            &[],
        ),
        Err(DbError::RecursiveView(_))
    ));
}

#[test]
fn cte_shadow_existing_table_errors() {
    let mut d = db();
    assert!(matches!(
        d.execute(
            &parse_statement("WITH t AS (SELECT id FROM t) SELECT * FROM t").unwrap(),
            &[],
        ),
        Err(DbError::ViewAlreadyExists(_))
    ));
}

#[test]
fn cte_visible_in_subquery_from() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // CTE "adults" is defined in the outer query; the subquery in FROM
    // should be able to reference it.
    let r = d
        .execute(
            &parse_statement(
                "WITH adults AS (SELECT id, age FROM t WHERE age >= 20) \
                 SELECT * FROM (SELECT id FROM adults) AS sub",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn cte_visible_in_where_subquery() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // CTE "adults" is used in an EXISTS subquery in WHERE.
    let r = d
        .execute(
            &parse_statement(
                "WITH adults AS (SELECT id FROM t WHERE age >= 20) \
                 SELECT id FROM t WHERE EXISTS (SELECT id FROM adults)",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    // CTE is non-empty so EXISTS is true for all 5 outer rows.
    assert_eq!(r.rows.len(), 5);
}

#[test]
fn correlated_subquery_sees_outer_row() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // Non-correlated EXISTS works.
    let r = d
        .execute(
            &parse_statement("SELECT id FROM t WHERE EXISTS (SELECT id FROM t WHERE id = 1)")
                .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 5);

    // Correlated: inner references outer.id via column-to-column comparison.
    let r2 = d
        .execute(
            &parse_statement(
                "SELECT id FROM t WHERE EXISTS (SELECT id FROM t AS inner_t WHERE inner_t.id < t.id)",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r2.rows.len(), 4);
}

#[test]
fn column_to_column_comparison() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // Simple column-to-column without alias.
    let r = d
        .execute(
            &parse_statement("SELECT id FROM t WHERE id < age").unwrap(),
            &[],
        )
        .unwrap();
    // id < age: id=0 age=0 no, id=1 age=10 yes, id=2 age=20 yes, ...
    assert_eq!(r.rows.len(), 4);
}

#[test]
fn correlated_subquery_two_levels_deep() {
    let mut d = db();
    for i in 0..5 {
        let sql = alloc::format!(
            "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
            i * 10
        );
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // Verify nested EXISTS works (non-correlated).
    let r = d
        .execute(
            &parse_statement("SELECT id FROM t WHERE EXISTS (SELECT id FROM t WHERE id = 1)")
                .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 5);

    // Two-level correlation: outer → mid → inner.
    // For each outer row `t`, check if there exists a mid row where
    // mid.id > t.id AND that mid row has an inner row with
    // inner.id > mid.id.  Returns true for id=0..2 (0<1<2, 0<1<3, etc.).
    let r2 = d
        .execute(
            &parse_statement(
                "SELECT id FROM t WHERE EXISTS \
                 (SELECT id FROM t AS mid WHERE mid.id > t.id \
                  AND EXISTS \
                  (SELECT id FROM t AS inner_t WHERE inner_t.id > mid.id))",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    // id=0: mid can be 1,2,3,4; inner exists (e.g. mid=1 inner=2) ✓
    // id=1: mid can be 2,3,4; inner exists (e.g. mid=2 inner=3) ✓
    // id=2: mid can be 3,4; inner exists (e.g. mid=3 inner=4) ✓
    // id=3: mid=4; no inner > 4 ✗
    // id=4: no mid > 4 ✗
    assert_eq!(r2.rows.len(), 3);
}

#[test]
fn alter_table_add_column() {
    let mut d = db();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("ALTER TABLE t ADD COLUMN email TEXT").unwrap(),
        &[],
    )
    .unwrap();
    let r = d
        .execute(&parse_statement("SELECT * FROM t").unwrap(), &[])
        .unwrap();
    assert_eq!(r.columns.len(), 4); // id, name, age, email
    assert_eq!(r.rows[0][3], Value::Null); // default value
}

#[test]
fn alter_table_drop_column() {
    let mut d = db();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("ALTER TABLE t DROP COLUMN age").unwrap(),
        &[],
    )
    .unwrap();
    let r = d
        .execute(&parse_statement("SELECT * FROM t").unwrap(), &[])
        .unwrap();
    assert_eq!(r.columns.len(), 2); // id, name only
    assert_eq!(r.rows[0][0], Value::Int(1));
    assert_eq!(r.rows[0][1], Value::Text("alice".to_string()));
}

#[test]
fn alter_table_rename_column() {
    let mut d = db();
    d.execute(
        &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("ALTER TABLE t RENAME COLUMN name TO full_name").unwrap(),
        &[],
    )
    .unwrap();
    let r = d
        .execute(&parse_statement("SELECT * FROM t").unwrap(), &[])
        .unwrap();
    assert_eq!(r.columns[1], "full_name");
}

#[test]
fn alter_table_unknown_column_errors() {
    let mut d = db();
    assert!(matches!(
        d.execute(
            &parse_statement("ALTER TABLE t DROP COLUMN nonexistent").unwrap(),
            &[],
        ),
        Err(DbError::UnknownColumn(_))
    ));
}

#[test]
fn alter_table_add_duplicate_column_errors() {
    let mut d = db();
    assert!(matches!(
        d.execute(
            &parse_statement("ALTER TABLE t ADD COLUMN name TEXT").unwrap(),
            &[],
        ),
        Err(DbError::Unsupported(_))
    ));
}

#[test]
fn alter_table_unknown_table_errors() {
    let mut d = db();
    assert!(matches!(
        d.execute(
            &parse_statement("ALTER TABLE nonexistent ADD COLUMN x INT").unwrap(),
            &[],
        ),
        Err(DbError::UnknownTable(_))
    ));
}

#[test]
fn having_filters_groups() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE t (dept TEXT, salary INT)").unwrap(),
        &[],
    )
    .unwrap();
    for (dept, salary) in [("eng", 100), ("eng", 200), ("sales", 150), ("sales", 50)] {
        let sql = alloc::format!("INSERT INTO t (dept, salary) VALUES ('{dept}', {salary})");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement("SELECT dept, COUNT(*) AS cnt FROM t GROUP BY dept HAVING cnt >= 2")
                .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn having_filters_groups_with_sum() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE t (dept TEXT, salary INT)").unwrap(),
        &[],
    )
    .unwrap();
    for (dept, salary) in [("eng", 100), ("eng", 200), ("sales", 150), ("sales", 50)] {
        let sql = alloc::format!("INSERT INTO t (dept, salary) VALUES ('{dept}', {salary})");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement(
                "SELECT dept, SUM(salary) AS total FROM t GROUP BY dept HAVING total > 200",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    // eng total = 300 (>200), sales total = 200 (not >200)
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("eng".to_string()));
}

#[test]
fn having_without_group_by_filters_single_aggregate() {
    let mut d = Database::empty();
    d.execute(&parse_statement("CREATE TABLE t (x INT)").unwrap(), &[])
        .unwrap();
    for i in 0..5 {
        let sql = alloc::format!("INSERT INTO t (x) VALUES ({i})");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    // COUNT(*) = 5, HAVING 5 > 10 should filter it out.
    let r = d
        .execute(
            &parse_statement("SELECT COUNT(*) AS cnt FROM t HAVING cnt > 10").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 0);
}

#[test]
fn having_with_and_or() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE t (dept TEXT, salary INT)").unwrap(),
        &[],
    )
    .unwrap();
    for (dept, salary) in [("eng", 100), ("eng", 200), ("sales", 150), ("hr", 50)] {
        let sql = alloc::format!("INSERT INTO t (dept, salary) VALUES ('{dept}', {salary})");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement(
                "SELECT dept, COUNT(*) AS cnt FROM t GROUP BY dept HAVING cnt >= 2 AND cnt <= 2",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    // Only eng has cnt=2.
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Text("eng".to_string()));
}

#[test]
fn uncorrelated_exists_is_cached() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE t (id INT, name TEXT)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(&parse_statement("CREATE TABLE t2 (val INT)").unwrap(), &[])
        .unwrap();
    for i in 0..5 {
        let sql = alloc::format!("INSERT INTO t (id, name) VALUES ({i}, 'u{i}')");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    d.execute(
        &parse_statement("INSERT INTO t2 (val) VALUES (10)").unwrap(),
        &[],
    )
    .unwrap();
    // Uncorrelated EXISTS — inner query doesn't reference outer columns.
    // Should return all rows since t2 has at least one row.
    let r = d
        .execute(
            &parse_statement("SELECT id FROM t WHERE EXISTS (SELECT val FROM t2 WHERE val > 5)")
                .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 5);
    // No matching row: t2.val = 10, not < 5.
    let r2 = d
        .execute(
            &parse_statement("SELECT id FROM t WHERE EXISTS (SELECT val FROM t2 WHERE val < 5)")
                .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r2.rows.len(), 0);
}

#[test]
fn uncorrelated_in_subquery_is_cached() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE t (id INT, name TEXT)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(&parse_statement("CREATE TABLE t2 (val INT)").unwrap(), &[])
        .unwrap();
    for i in 0..5 {
        let sql = alloc::format!("INSERT INTO t (id, name) VALUES ({i}, 'u{i}')");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    d.execute(
        &parse_statement("INSERT INTO t2 (val) VALUES (1)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(
        &parse_statement("INSERT INTO t2 (val) VALUES (3)").unwrap(),
        &[],
    )
    .unwrap();
    // Uncorrelated IN subquery.
    let r = d
        .execute(
            &parse_statement("SELECT id FROM t WHERE id IN (SELECT val FROM t2)").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], Value::Int(1));
    assert_eq!(r.rows[1][0], Value::Int(3));
}

#[test]
fn mixed_correlated_uncorrelated_subqueries() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE t (id INT, name TEXT)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(&parse_statement("CREATE TABLE t2 (val INT)").unwrap(), &[])
        .unwrap();
    for i in 0..5 {
        let sql = alloc::format!("INSERT INTO t (id, name) VALUES ({i}, 'u{i}')");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    d.execute(
        &parse_statement("INSERT INTO t2 (val) VALUES (10)").unwrap(),
        &[],
    )
    .unwrap();
    // Both correlated AND uncorrelated subqueries in one WHERE.
    // uncorrelated: EXISTS (t2 WHERE val > 5) → always true
    // correlated: t.id < 3 → filters to id 0,1,2
    let r = d
        .execute(
            &parse_statement(
                "SELECT id FROM t WHERE EXISTS \
                 (SELECT val FROM t2 WHERE val > 5) \
                 AND id < 3",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0][0], Value::Int(0));
    assert_eq!(r.rows[1][0], Value::Int(1));
    assert_eq!(r.rows[2][0], Value::Int(2));
}

#[test]
fn uncorrelated_scalar_subquery_is_cached() {
    let mut d = Database::empty();
    d.execute(
        &parse_statement("CREATE TABLE t (id INT, name TEXT)").unwrap(),
        &[],
    )
    .unwrap();
    d.execute(&parse_statement("CREATE TABLE t2 (val INT)").unwrap(), &[])
        .unwrap();
    for i in 0..5 {
        let sql = alloc::format!("INSERT INTO t (id, name) VALUES ({i}, 'u{i}')");
        d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
    }
    d.execute(
        &parse_statement("INSERT INTO t2 (val) VALUES (3)").unwrap(),
        &[],
    )
    .unwrap();
    // Uncorrelated scalar subquery: id > (SELECT val FROM t2) → id > 3 → id=4 only.
    let r = d
        .execute(
            &parse_statement("SELECT id FROM t WHERE id > (SELECT val FROM t2)").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(4));
}

#[test]
fn with_recursive_hierarchy_traversal() {
    // Classic adjacency-list traversal: nodes(node_id, parent), root has a
    // NULL parent. The recursive term walks one generation per iteration.
    let mut d = Database::empty();
    for sql in [
        "CREATE TABLE nodes (node_id INT, parent INT)",
        "INSERT INTO nodes (node_id, parent) VALUES (1, NULL)",
        "INSERT INTO nodes (node_id, parent) VALUES (2, 1)",
        "INSERT INTO nodes (node_id, parent) VALUES (3, 1)",
        "INSERT INTO nodes (node_id, parent) VALUES (4, 2)",
    ] {
        d.execute(&parse_statement(sql).unwrap(), &[]).unwrap();
    }
    let r = d
        .execute(
            &parse_statement(
                "WITH RECURSIVE tree AS ( \
                    SELECT node_id, parent FROM nodes WHERE parent IS NULL \
                    UNION ALL \
                    SELECT node_id, parent FROM nodes AS n JOIN tree AS t ON n.parent = t.node_id \
                 ) SELECT node_id FROM tree",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
    let mut ids: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Int(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, alloc::vec![1, 2, 3, 4]);
}

#[test]
fn with_recursive_non_self_referencing_cte_still_works() {
    // A CTE inside a `WITH RECURSIVE` clause that doesn't actually
    // self-reference is legal (just an ordinary CTE).
    let mut d = Database::empty();
    d.execute(&parse_statement("CREATE TABLE t (v INT)").unwrap(), &[])
        .unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (v) VALUES (7)").unwrap(),
        &[],
    )
    .unwrap();
    let r = d
        .execute(
            &parse_statement("WITH RECURSIVE cte AS (SELECT v FROM t) SELECT v FROM cte").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows, alloc::vec![alloc::vec![Value::Int(7)]]);
}

#[test]
fn with_recursive_non_terminating_hits_iteration_cap() {
    // A recursive term with no base case that shrinks the working set never
    // reaches a fixed point; this must surface as a `DbError`, not hang or
    // stack-overflow the process.
    let mut d = Database::empty();
    d.execute(&parse_statement("CREATE TABLE t (v INT)").unwrap(), &[])
        .unwrap();
    d.execute(
        &parse_statement("INSERT INTO t (v) VALUES (1)").unwrap(),
        &[],
    )
    .unwrap();
    let r = d.execute(
        &parse_statement(
            "WITH RECURSIVE cte AS ( \
                SELECT v FROM t \
                UNION ALL \
                SELECT v FROM cte \
             ) SELECT v FROM cte",
        )
        .unwrap(),
        &[],
    );
    assert!(matches!(r, Err(DbError::Unsupported(_))));
}

#[test]
fn cte_self_reference_hidden_in_where_subquery_is_rejected() {
    // Closes the stack-overflow hole: `select_references_table` used to only
    // walk FROM/JOIN, so a self-reference hidden inside a WHERE-clause
    // subquery went undetected and recursed forever at execution time.
    let mut d = Database::empty();
    d.execute(&parse_statement("CREATE TABLE t (v INT)").unwrap(), &[])
        .unwrap();
    let r = d.execute(
        &parse_statement(
            "WITH cte AS (SELECT v FROM t WHERE v IN (SELECT v FROM cte)) SELECT v FROM cte",
        )
        .unwrap(),
        &[],
    );
    assert!(matches!(r, Err(DbError::RecursiveView(_))));
}

fn parse(s: &str) -> crate::parser::Statement {
    crate::parser::parse_statement(s).unwrap()
}

#[test]
fn extract_year_from_date_column() {
    let mut d = Database::empty();
    d.execute(&parse("CREATE TABLE events (id INT, happened DATE)"), &[])
        .unwrap();
    // 2024-06-15 = 19889 days since epoch (approx)
    d.execute(
        &parse("INSERT INTO events (id, happened) VALUES (1, 19889)"),
        &[],
    )
    .unwrap();
    // 2023-01-01 = 19358 days since epoch (approx)
    d.execute(
        &parse("INSERT INTO events (id, happened) VALUES (2, 19358)"),
        &[],
    )
    .unwrap();
    let r = d
        .execute(
            &parse("SELECT id FROM events WHERE EXTRACT(YEAR FROM happened) = 2024"),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));
}

#[test]
fn extract_month_from_date_column() {
    let mut d = Database::empty();
    d.execute(&parse("CREATE TABLE events (id INT, happened DATE)"), &[])
        .unwrap();
    // March = month 3. Days since epoch for 2024-03-15...
    // 2024-03-15: year=2024, month=3, day=15
    // 2024-01-01 = 19723 days since epoch
    // 2024-03-15 = 19723 + 31 (Jan) + 29 (Feb leap) + 14 = 19797
    d.execute(
        &parse("INSERT INTO events (id, happened) VALUES (1, 19797)"),
        &[],
    )
    .unwrap();
    // 2024-06-15 = 19723 + 31 + 29 + 31 + 30 + 31 + 14 = 19889
    d.execute(
        &parse("INSERT INTO events (id, happened) VALUES (2, 19889)"),
        &[],
    )
    .unwrap();
    let r = d
        .execute(
            &parse("SELECT id FROM events WHERE EXTRACT(MONTH FROM happened) >= 4"),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(2));
}

#[test]
fn extract_is_null_on_nullable_date() {
    let mut d = Database::empty();
    d.execute(&parse("CREATE TABLE t (id INT, d DATE)"), &[])
        .unwrap();
    d.execute(&parse("INSERT INTO t (id, d) VALUES (1, NULL)"), &[])
        .unwrap();
    d.execute(&parse("INSERT INTO t (id, d) VALUES (2, 19723)"), &[])
        .unwrap();
    let r = d
        .execute(
            &parse("SELECT id FROM t WHERE EXTRACT(YEAR FROM d) IS NULL"),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));
}
