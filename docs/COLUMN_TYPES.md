# Column-type matrix

This is the authoritative mapping of `tpt-archon-relational`'s `ColumnType`
enum to SQL surface syntax and the PostgreSQL type it is advertised as over
the wire protocol. It closes the Phase 12.4 "broader, documented column-type
matrix" follow-up: the engine already covers the common types; this document
is the coverage map, not a correctness gap.

| `ColumnType`       | SQL syntax            | Storage                       | PG type name | PG OID |
|--------------------|-----------------------|-------------------------------|--------------|--------|
| `Int`              | `INT` / `INTEGER`     | `i64`                         | `int8`       | 20     |
| `Boolean`          | `BOOLEAN`             | `bool`                        | `bool`       | 16     |
| `Float`            | `FLOAT`               | `f32`                         | `float4`     | 700    |
| `Double`           | `DOUBLE`              | `f64`                         | `float8`     | 701    |
| `Numeric`          | `NUMERIC(p,s)`        | `i64` (precision deferred)    | `numeric`    | 1700   |
| `Text`             | `TEXT`                | UTF-8 bytes                   | `text`       | 25     |
| `Varchar(n)`       | `VARCHAR(n)`          | UTF-8 bytes (length checked)  | `varchar`    | 1043   |
| `Date`             | `DATE`                | `i64` days since epoch        | `date`       | 1082   |
| `Timestamp`        | `TIMESTAMP`           | `i64` micros since epoch      | `timestamp`  | 1114   |
| `Vector`           | `FLOAT[]` / `VECTOR`  | `Vec<f32>`                    | `text`*      | 25     |

`*` `VECTOR` is wire-encoded as `text` (OID 25) using pgvector's own
`[0.1,0.9]` textual form (Phase 8 B5 vector-type wire encoding decision), so
`psql` output is byte-identical to a real pgvector server for the same query.
pgvector's `vector` OID is not a fixed constant — it is discovered per
database via `pg_type` — so we avoid claiming a fixed `vector` OID.

The mapping lives in `crates/tpt-archon-relational/src/parser/ast.rs`
(`ColumnType::pg_type_name` / `ColumnType::pg_type_oid`) and is exercised by
`pg_catalog` emulation (Phase 12.3) and the wire protocol's `RowDescription`.

## Known coverage gaps (not yet implemented)

The following remain deferred (see Phase 12.3 "remaining dialect gaps") and are
**not** in the matrix above: `CHAR(n)`, `TIME`/`TIMETZ`, `INTERVAL`,
`JSON`/`JSONB`, `UUID`, `BYTEA` (raw bytes), and user-defined/`DOMAIN` types.
Broadening the matrix to these is a coverage follow-up, not a correctness
blocker — each new type needs a `ColumnType` variant plus codec round-trip
support in `database::codec`.
