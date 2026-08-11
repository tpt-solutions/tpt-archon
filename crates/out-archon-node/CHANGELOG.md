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
- Node.js bindings (`archon-node` package, `out-archon-node` crate) via napi-rs
  for `tpt-archon-relational`'s embeddable SQL engine: a thin wrapper where SQL
  text goes in via `execute()` and plain JS objects come out.
- `new Database()` plus `db.execute(sql, vectorParams?)` supporting the same SQL
  surface as the Rust engine and the `archon-sql` REPL, including the `VECTOR[dim]`
  column type and `ORDER BY cosine(col, ?) LIMIT k` nearest-neighbor search.
- `db.tableNames()` listing currently defined tables. Synchronous, in-memory for
  the process lifetime; ships to npm (not crates.io), hence the `out-archon-`
  prefix.
