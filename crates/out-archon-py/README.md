# archon-db

Python bindings ([PyO3](https://pyo3.rs)) for `tpt-archon-relational`'s
embeddable SQL engine — the same engine behind the `archon-sql` REPL, exposed
as a single `archon.Database` class so it can be adopted from Python without
touching Rust.

This is the crate's adoption story in one line: the vector-search/RAG
audience `ORDER BY cosine(...) LIMIT k` targets is overwhelmingly Python, so
this package puts that feature (and ordinary SQL) one `pip install` away.

## Install (local development)

This package is **not yet published to PyPI**. To build and use it locally:

```sh
pip install maturin
cd crates/out-archon-py
maturin develop        # builds the extension module and installs it into your active venv
```

`maturin develop` needs a Python interpreter (3.8+) and a Rust toolchain; it
compiles the `cdylib` in this crate and installs it as the `archon` module.

## Usage

```python
import archon

db = archon.Database()

db.execute("CREATE TABLE users (name TEXT, age INT)")
db.execute("INSERT INTO users (name, age) VALUES ('alice', 30)")
db.execute("INSERT INTO users (name, age) VALUES ('bob', 25)")

rows = db.execute("SELECT name, age FROM users WHERE age >= 25 ORDER BY age")
print(rows)
# [{'name': 'bob', 'age': 25}, {'name': 'alice', 'age': 30}]

db.execute("UPDATE users SET age = 31 WHERE name = 'alice'")
db.execute("DELETE FROM users WHERE name = 'bob'")
```

Every table has an implicit leading `id INT` column, matching the underlying
Rust engine (`CREATE TABLE` there always prepends one).

### Vector search (`ORDER BY cosine(...) LIMIT k`)

```python
import archon

db = archon.Database()
db.execute("CREATE TABLE docs (text TEXT, embedding VECTOR[3])")
db.execute("INSERT INTO docs (text, embedding) VALUES ('a', [1.0, 0.0, 0.0])")
db.execute("INSERT INTO docs (text, embedding) VALUES ('b', [0.0, 1.0, 0.0])")

query_vector = [0.9, 0.1, 0.0]
rows = db.execute(
    "SELECT text FROM docs ORDER BY cosine(embedding, ?) LIMIT 1",
    params=[query_vector],
)
print(rows)  # [{'text': 'a'}]
```

`params` is a list of `f32` vectors, one per `?` placeholder in the
statement's `ORDER BY cosine(...)` clause (currently the only place `?`
placeholders are used) — pass `params=[query_vector]` for a single
placeholder.

### Introspection

```python
db.tables()          # -> ['users']
db.schema('users')   # -> [('id', 'INT'), ('name', 'TEXT'), ('age', 'INT')]
```

### Errors

A SQL syntax error raises `ValueError`; a runtime error (unknown table,
unknown column, arity mismatch, transaction conflict, etc.) raises
`RuntimeError` with the same wording `archon-sql` prints.

## What this is

- A thin wrapper around `tpt_archon_relational::database::Database`: every
  `execute()` call parses the SQL string and runs it through the exact same
  `Database::execute` path the `archon-sql` REPL and the Rust test suite use.
- In-memory only: each `archon.Database()` instance holds its tables for the
  lifetime of the Python object. There is no file-backed / persistent
  constructor exposed yet, even though `tpt-archon-core` supports file-backed
  storage — wiring that through is future work.
- Single-threaded, synchronous: `execute()` blocks the calling Python thread;
  there is no async/await support.
- Rows come back as a `list[dict[str, ...]]` per call — one dict per row,
  keyed by output column name. `INT` -> `int`, `TEXT` -> `str`, `VECTOR` ->
  `list[float]`, `NULL` -> `None`.

## What this is not (yet)

- **Not published to PyPI.** `pip install archon-db` doesn't work yet; use
  `maturin develop` from a source checkout.
- **No transactions from Python yet.** `BEGIN`/`COMMIT`/`ROLLBACK` are parsed
  and executed by the underlying engine, so `db.execute("BEGIN")` etc. will
  work, but there's no Pythonic context-manager wrapper (`with db.transaction():`)
  around it yet.
- **No DB-API 2.0 (PEP 249) compliance.** There's no `Connection`/`Cursor`
  split, no `executemany`, no parameter-substitution for ordinary literals
  (only for the vector `?` placeholders `ORDER BY cosine(...)` uses) — build
  SQL strings directly for now.
- **No wheels built yet.** There's no prebuilt binary for any platform;
  everything here is source-build-only via `maturin develop`/`maturin build`.

See the root [`README.md`](../../README.md) and
[`docs/GETTING_STARTED.md`](../../docs/GETTING_STARTED.md) for the equivalent
Rust-side API and the engine's overall status/limitations.
