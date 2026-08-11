# archon-sql

Interactive SQL REPL for the [TPT Archon](https://github.com/tpt-solutions/tpt-archon)
database engine. Package name `out-archon-sql`; binary name `archon-sql`. Not
published (`publish = false`) — a dev/demo binary, not one of the layered
architecture crates (see `AGENTS.md`'s publish/no-publish naming convention).

It is a thin, friendly front-end over
[`tpt-archon-relational`](../tpt-archon-relational)'s `Database`: the same
parser, planner, and executor the engine's Rust tests use, with a terminal
friendly result renderer on top.

## Usage

```sh
# interactive REPL
cargo run -p out-archon-sql

# run a single statement and exit
cargo run -p out-archon-sql -e "SELECT 1 + 2;"

# show usage / version
cargo run -p out-archon-sql --help
cargo run -p out-archon-sql --version
```

Inside the REPL, type `;` to terminate a statement and `SELECT`/`INSERT`/... it
runs immediately. Dot-commands begin with `.`:

```sql
CREATE TABLE users (id INT, name TEXT, age INT);
INSERT INTO users (id, name, age) VALUES (1, 'alice', 30);
INSERT INTO users (id, name, age) VALUES (2, 'bob', 25);
SELECT name, age FROM users WHERE age >= 25 ORDER BY age;
```

```
.name          print the current output mode
.mode table    render results as an aligned table (default)
.mode csv      render results as comma-separated values
.mode json     render results as JSON arrays
.help          list dot-commands
.quit          exit the REPL
```

## Features

- `sqlite-import` (optional) — enables `.import` of an existing SQLite database
  via the bundled `rusqlite` backend, so you can load real data into the engine
  for experimentation. Off by default.

## Status

The REPL is in-memory: each invocation starts from an empty `Database` and
discards it on exit (there is no `--file` / persistence flag wired up yet).
Everything else mirrors the underlying engine — see the root [`README.md`](../../README.md)
and [`TODO.md`](../../TODO.md) for what the engine supports (vector search via
`ORDER BY cosine(...) LIMIT k`, transactions, joins, aggregates) versus what
remains deferred.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
