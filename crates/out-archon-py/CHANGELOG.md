# Changelog

All notable changes to this crate are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Fixed

## [0.1.0] - 2026-08-05

### Added
- Python bindings (`archon-db` package, `out-archon-py` crate) via PyO3 for
  `tpt-archon-relational`'s embeddable SQL engine, exposed as a single
  `archon.Database` class — the same `parse_statement` + `execute` path the
  `archon-sql` REPL uses.
- `db.execute(sql)` for DDL/DML/`SELECT`, including vector search via
  `ORDER BY cosine(...) LIMIT k` with a `params=[...]` list of `f32` vectors.
- Introspection: `db.tables()` and `db.schema(name)`. Errors raise `ValueError`
  (syntax) or `RuntimeError` (runtime) with the same wording `archon-sql` prints.
- Rows returned as `list[dict]`; `INT`→`int`, `TEXT`→`str`, `VECTOR`→`list[float]`,
  `NULL`→`None`. In-memory, single-threaded, synchronous.
