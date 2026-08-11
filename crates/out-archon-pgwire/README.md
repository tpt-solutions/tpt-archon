# out-archon-pgwire

PostgreSQL wire-protocol server for
[TPT Archon](https://github.com/tpt-solutions/tpt-archon). Package name
`out-archon-pgwire`; binary name `archon-pgwire`. Not published
(`publish = false`) — a demo/verification tool, not one of the layered
architecture crates.

It implements enough of the real PostgreSQL frontend/backend protocol that any
Postgres client — `psql`, language drivers, ORMs — can talk to Archon directly,
without going through the `archon-sql` REPL. Its wire-level behavior is checked
against real Postgres by the [`out-archon-pgcompat`](../out-archon-pgcompat)
oracle (Phase 8 / Track C).

## Quick start

```sh
cargo run -p out-archon-pgwire --bin archon-pgwire
```

Listens on `127.0.0.1:5432` by default (override with the `HOST`/`PORT` env
vars) and accepts unauthenticated (`trust`) connections. Then, from another
terminal:

```sh
psql -h 127.0.0.1 -p 5432 -U postgres
```

## Modules

- [`codec`](src/codec) — the wire framing: `MessageReader`/`MessageWriter` over
  the PostgreSQL message types and the v3 protocol byte layout.
- [`startup`](src/startup.rs) — startup/authentication handshake (`StartupMessage`,
  `trust` auth; SCRAM is deferred).
- [`simple_query`](src/simple_query.rs) — the simple query protocol (`Q`):
  runs a statement through `tpt-archon-relational` and renders `RowDescription`
  + `DataRow` + `CommandComplete`/`ErrorResponse`.
- [`extended`](src/extended.rs) — stubs for the extended query protocol
  (`Parse`/`Bind`/`Execute`/`Describe`/`Sync`).
- [`session`](src/session.rs) — per-connection session state (current database,
  transaction state, prepared statements).
- [`sqlstate`](src/sqlstate.rs) — a **deliberately exhaustive** `match` mapping
  the relational engine's `DbError`/`ExecError` to PostgreSQL SQLSTATE codes.
  Adding, renaming, or removing a variant there **must** be mirrored here, or
  the workspace fails to compile (see `AGENTS.md`'s crate-sync hazard note).
- [`error`](src/error.rs) — the server's own error type and `ErrorResponse`
  rendering.
- [`catalog`](src/catalog.rs) — minimal `pg_catalog`-style introspection the
  wire protocol expects (limited; full `pg_catalog` emulation is deferred).
- [`compat`](src/compat.rs) — type/format negotiation glue between Archon
  `Value`s and Postgres wire types.
- [`metrics`](src/metrics.rs) — connection / statement counters surfaced for
  smoke tests.
- [`backup`](src/backup.rs) + `archon-backup` binary — an offline snapshot/backup
  helper for the on-disk `Database`.
- [`tls`](src/tls.rs) (opt-in `tls` feature) — optional rustls-backed TLS
  transport for the wire server.

## Architecture note (ADR 0004)

Concurrency is capped at a mutex regardless of connection count because
`Database` is not `Sync + Send` in the current single-threaded arena model. The
server is a thread-per-connection `TcpListener` sharing one `Database` behind an
`Arc<Mutex<_>>`; adding `tokio` or another async runtime buys nothing for
connection scaling until the storage layer becomes `Sync + Send`. Revisit on
measured evidence, not taste.

## Features

- `std` (default) — enables the network server (needs `std::net`).
- `tls` — enables the optional rustls TLS transport (off by default).

## Status

Coverage is narrower than real Postgres — see [`TODO.md`](../../TODO.md)'s
Phase 8 for what's supported (simple + extended query protocol, SQLSTATE errors,
transactions) versus deferred (`pg_catalog` emulation, SCRAM auth, `COPY`, TLS
by default).

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
