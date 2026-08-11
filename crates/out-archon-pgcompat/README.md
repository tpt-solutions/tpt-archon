# out-archon-pgcompat

Real-PostgreSQL oracle for
[`tpt-archon-relational`](../tpt-archon-relational)'s `.slt` comparison corpus
(Phase 8 / Track C, "slice 1" of the PostgreSQL-compatibility roadmap — see
`TODO.md` and the plan doc). Not published (`publish = false`).

It is the *real-Postgres* half of the compatibility story. The relational
crate's `tests/slt.rs` runs the same corpus against Archon's own `Database`
directly (a normal `cargo test --workspace` integration test, zero new deps,
always runs). This binary runs the corpus's `supported/*.slt` against a **live
PostgreSQL** server and asserts the expected rows are what Postgres actually
produces — which is what turns the corpus into a validated oracle rather than a
mere record of Archon's own behavior.

## Corpus location

It reads the *same* files `tests/slt.rs` reads — this binary does not vendor or
duplicate corpus content — at `../tpt-archon-relational/tests/slt/` relative to
`CARGO_MANIFEST_DIR`.

## How to run

This crate is intentionally **excluded** from the root workspace (see the root
`Cargo.toml` and this crate's own `Cargo.toml` comment): its `postgres` dependency
pulls in `tokio-postgres` transitively, and a bare `cargo test --workspace` has
no Postgres to talk to. It is only ever run by the opt-in `pg-compat` CI job or
manually against a local `docker compose up postgres`.

```sh
docker compose up -d postgres            # pgvector/pgvector:pg16, --locale=C
cd crates/out-archon-pgcompat
POSTGRES_URL=postgres://postgres:postgres@localhost:5432/postgres cargo run --release
```

If `POSTGRES_URL` is unset it exits cleanly (no failure), mirroring the corpus
tests' skip pattern elsewhere in the workspace.

## Design notes

- **`divergent/` is skipped.** Those files document Archon's *own* divergences
  from Postgres; there is nothing to validate about Postgres there (by
  definition Postgres is not expected to match a documented Archon gap). A
  divergence-aware comparison mode was explicitly deferred by the slice-1 spec.
- **`statement error`: existence-of-error only.** The corpus's `<pattern>`
  substrings are Archon-internal `DbError` identifiers (`TableAlreadyExists`,
  `UnknownColumn`, ...), not portable Postgres text. This oracle only asserts
  that Postgres *also* raises an error for such directives; it does not pattern
  match Postgres's message.
- **Type-agnostic row comparison.** Every query is wrapped as
  `SELECT row_to_json(t)::text FROM (<original query>) AS t`, yielding a single
  `text` column. A small hand-rolled flat-JSON-value scanner pulls value tokens
  in column order, formatted the same way `tests/slt.rs` renders Archon's own
  `Value`, so the two oracles compare apples to apples.
- **Why a second small `.slt` parser?** This crate is excluded from the workspace
  and must not path-depend on `tpt-archon-relational`'s test-only directive
  parser. Per the slice-1 spec, a small amount of duplicated parsing logic
  between the two is acceptable; both are kept deliberately minimal.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
