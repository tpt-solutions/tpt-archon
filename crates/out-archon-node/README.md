# archon-node

Node.js bindings ([napi-rs](https://napi.rs)) for `tpt-archon-relational`'s
embeddable SQL engine. This is the package name `archon-node`; the crate behind
it is `out-archon-node` (see the naming-convention note in the root
`CLAUDE.md`: it ships to npm, not crates.io, so it isn't published to
crates.io and carries the `out-archon-` prefix there).

## What this is

A thin wrapper around
[`tpt_archon_relational::database::Database`](../tpt-archon-relational/src/database.rs):
SQL text goes in via `execute()`, plain JS objects come out. It supports the
same SQL surface as the Rust engine and the `archon-sql` REPL --
`CREATE TABLE`/`ALTER TABLE`, `INSERT`/`SELECT`/`UPDATE`/`DELETE`,
`WHERE`/`JOIN`/`GROUP BY`/`ORDER BY`, `BEGIN`/`COMMIT`/`ROLLBACK`, and the
vector column type (`VECTOR[dim]`) with `ORDER BY cosine(col, ?) LIMIT k`
for RAG-style nearest-neighbor search.

## What this is not (yet)

- **Not published to npm.** There is no `npm install archon-node` yet --
  build it from source (below).
- **No async/Promise API.** `execute()` is synchronous; there's no
  worker-thread offload, so a long-running query blocks the Node event loop.
- **No file-backed persistence from Node.** Every `Database` is in-memory
  for the process lifetime (same as `Database::empty()` in Rust); opening an
  existing on-disk file isn't wired up on this binding.
- **No general parameter binding.** `execute(sql, vectorParams)`'s second
  argument only supplies the query vector(s) for
  `ORDER BY cosine(col, ?) LIMIT k` (mirroring the underlying Rust API's
  `params: &[Vec<f32>]` exactly) -- it is not a `?`-placeholder mechanism for
  ordinary int/text values. Those go directly in the SQL text.
- **Only prebuilt for the platforms listed in `package.json`'s `napi.targets`**
  once someone actually runs the cross-compile matrix; today, building means
  compiling locally for whatever platform you're on.

## Build

Requires a stable Rust toolchain (>= 1.77 -- see the version note in
`Cargo.toml`) and Node.js >= 16.

```sh
cd crates/out-archon-node
npm install
npm run build     # napi build --platform --release
```

This produces a platform-specific `archon-node.<platform>.node` native module
plus generated `index.js` / `index.d.ts` loader files in this directory.

Run the smoke test against the freshly built module:

```sh
npm test          # node smoke-test.js
```

## Usage

```js
const { Database } = require("archon-node");

const db = new Database();

db.execute("CREATE TABLE users (name TEXT, age INT)");
db.execute("INSERT INTO users (name, age) VALUES ('alice', 30)");
db.execute("INSERT INTO users (name, age) VALUES ('bob', 25)");

const rows = db.execute(
  "SELECT name, age FROM users WHERE age >= 25 ORDER BY age"
);
console.log(rows);
// [ { name: 'bob', age: 25 }, { name: 'alice', age: 30 } ]
```

Vector search (`ORDER BY cosine(...) LIMIT k`):

```js
db.execute("CREATE TABLE docs (label TEXT, embedding VECTOR[3])");
db.execute(
  "INSERT INTO docs (label, embedding) VALUES ('close', [1.0, 0.0, 0.0])"
);
db.execute(
  "INSERT INTO docs (label, embedding) VALUES ('far', [0.0, 1.0, 0.0])"
);

const nearest = db.execute(
  "SELECT label FROM docs ORDER BY cosine(embedding, ?) LIMIT 1",
  [[1.0, 0.0, 0.0]]
);
console.log(nearest); // [ { label: 'close' } ]
```

## API

- `new Database()` -- creates an empty database. There's no schema-object
  constructor; the underlying Rust API's own docs mark the schema-taking
  constructor "legacy" and recommend `Database::empty()` + `CREATE TABLE`
  instead, so that's what this binding does too.
- `db.execute(sql: string, vectorParams?: number[][]): Record<string, any>[]`
  -- runs one statement, returns its rows as plain objects keyed by column
  name (`[]` for DDL/DML that doesn't produce rows). Throws a JS `Error` on
  a parse error or a database error (unknown table/column, arity mismatch,
  transaction conflict, etc).
- `db.tableNames(): string[]` -- names of all tables currently defined.

### A note on numbers

SQL `INT` columns are 64-bit in the engine but surface as JS `number`
(`f64`), so integers beyond +/-2^53 lose precision round-tripping through
this binding -- a real limitation of mapping onto JS's single numeric type,
not an oversight.

## Status

Early -- this crate exists to make the engine easy to try from Node, not as
a finished product. See the root `README.md`'s Status section and `TODO.md`
for what's implemented across the wider workspace.
