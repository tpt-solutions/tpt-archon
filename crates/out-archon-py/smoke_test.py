"""Smoke test for the `archon` PyO3 extension module.

Run after `maturin develop` (or `maturin develop --release`) has built and
installed the module into the active virtualenv:

    python smoke_test.py

Exercises CREATE TABLE, INSERT, SELECT, UPDATE, DELETE, and the vector
`ORDER BY cosine(...) LIMIT k` path — not a full test suite, just enough to
catch a broken build or a binding-layer regression.
"""

import archon


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def main():
    db = archon.Database()

    db.execute("CREATE TABLE users (name TEXT, age INT)")
    check(db.tables() == ["users"], f"expected ['users'], got {db.tables()}")
    check(
        db.schema("users") == [("id", "INT"), ("name", "TEXT"), ("age", "INT")],
        f"unexpected schema: {db.schema('users')}",
    )

    db.execute("INSERT INTO users (name, age) VALUES ('alice', 30)")
    db.execute("INSERT INTO users (name, age) VALUES ('bob', 25)")

    rows = db.execute("SELECT name, age FROM users WHERE age >= 25 ORDER BY age")
    check(
        rows == [{"name": "bob", "age": 25}, {"name": "alice", "age": 30}],
        f"unexpected SELECT result: {rows}",
    )

    db.execute("UPDATE users SET age = 31 WHERE name = 'alice'")
    rows = db.execute("SELECT age FROM users WHERE name = 'alice'")
    check(rows == [{"age": 31}], f"UPDATE didn't take effect: {rows}")

    db.execute("DELETE FROM users WHERE name = 'bob'")
    rows = db.execute("SELECT name FROM users")
    check(rows == [{"name": "alice"}], f"DELETE didn't take effect: {rows}")

    # Vector search: ORDER BY cosine(...) LIMIT k.
    db.execute("CREATE TABLE docs (text TEXT, embedding VECTOR[3])")
    db.execute("INSERT INTO docs (text, embedding) VALUES ('a', [1.0, 0.0, 0.0])")
    db.execute("INSERT INTO docs (text, embedding) VALUES ('b', [0.0, 1.0, 0.0])")
    rows = db.execute(
        "SELECT text FROM docs ORDER BY cosine(embedding, ?) LIMIT 1",
        params=[[0.9, 0.1, 0.0]],
    )
    check(rows == [{"text": "a"}], f"vector search returned unexpected result: {rows}")

    # Error mapping: a SQL syntax error should raise ValueError, not crash.
    try:
        db.execute("SELECT ( FROM")
        raise AssertionError("expected a ValueError for invalid SQL")
    except ValueError:
        pass

    # Error mapping: a runtime error (unknown table) should raise RuntimeError.
    try:
        db.execute("SELECT * FROM nope")
        raise AssertionError("expected a RuntimeError for an unknown table")
    except RuntimeError:
        pass

    print("smoke test passed.")


if __name__ == "__main__":
    main()
