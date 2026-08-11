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
- PostgreSQL wire-protocol server (`archon-pgwire` binary) for
  `tpt-archon-relational`, so any Postgres client (`psql`, drivers, ORMs) can
  connect directly. Thread-per-connection `TcpListener` sharing one `Database`
  behind an `Arc<Mutex<_>>` (ADR 0004: mutex-capped until `Database` is
  `Sync + Send`).
- `codec` module — wire framing (`MessageReader`/`MessageWriter`) over the
  PostgreSQL v3 message types.
- `startup` module — startup/authentication handshake with `trust` auth
  (SCRAM deferred).
- `simple_query` module — the simple query protocol (`Q`); `extended` module —
  stubs for the extended query protocol (`Parse`/`Bind`/`Execute`/`Describe`/
  `Sync`).
- `session`, `catalog`, `compat`, `error`, `metrics` modules.
- `sqlstate` module — a deliberately exhaustive match mapping
  `tpt-archon-relational`'s `DbError`/`ExecError` to PostgreSQL SQLSTATE codes
  (adding/renaming/removing a variant there must be mirrored here or the
  workspace fails to compile).
- `backup` module + `archon-backup` binary — an offline snapshot/backup helper
  for the on-disk `Database`.
- Optional `tls` feature — rustls-backed TLS transport for the wire server.
