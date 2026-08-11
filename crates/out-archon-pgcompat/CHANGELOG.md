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
- Real-PostgreSQL oracle (`out-archon-pgcompat`) for
  `tpt-archon-relational`'s `.slt` comparison corpus (Phase 8 / Track C, slice 1).
  Runs the corpus's `supported/*.slt` against a live PostgreSQL server and
  asserts the expected rows are what Postgres actually produces, turning the
  corpus into a validated oracle.
- Reads the *same* `.slt` files `tests/slt.rs` reads (no vendoring/duplication).
- Type-agnostic row comparison via `row_to_json(t)::text`, parsed by a small
  hand-rolled flat-JSON-value scanner matching `tests/slt.rs`'s `display_value`
  formatting.
- `divergent/` corpus skipped entirely (documents Archon's own divergences, not
  Postgres behavior); `statement error` directives asserted as
  existence-of-error only (the corpus's `<pattern>` substrings are
  Archon-internal identifiers, not portable Postgres text).
- Excluded from the root workspace (its `postgres` dep pulls in `tokio-postgres`
  transitively and there is no Postgres in a bare `cargo test --workspace`);
  exits cleanly when `POSTGRES_URL` is unset.
