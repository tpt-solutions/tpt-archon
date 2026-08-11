# Examples cookbook

A use-case-indexed index of tpt-archon's runnable examples. If you're new to
the project, start with [`GETTING_STARTED.md`](GETTING_STARTED.md) instead —
it's a linear, step-by-step walkthrough. This page is the reference to come
back to once you know what you're trying to build and just need the right
entry point.

## Embed a database in your app

`tpt_archon_relational::database::Database` — parses SQL, plans it, and
executes it with the vectorized engine.

```sh
cargo run -p tpt-archon-relational --example select_end_to_end
```

Source: [`crates/tpt-archon-relational/examples/select_end_to_end.rs`](../crates/tpt-archon-relational/examples/select_end_to_end.rs)

## Use the storage engine directly

`tpt_archon_core::storage::Database` — the embeddable, file-backed storage
layer (block device + WAL + B-Link tree), without the SQL layer on top.

```sh
cargo run -p tpt-archon-core --example storage_tour
```

Source: [`crates/tpt-archon-core/examples/storage_tour.rs`](../crates/tpt-archon-core/examples/storage_tour.rs)

## Capability-scoped multi-tenant page cache

Two tenants share one unified page cache; each is issued a capability scoped
to its own pages, and cross-tenant access is denied.

```sh
cargo run -p tpt-archon-bridge --example multi_tenant
```

Source: [`crates/tpt-archon-bridge/examples/multi_tenant.rs`](../crates/tpt-archon-bridge/examples/multi_tenant.rs)

## Talk to Archon over the PostgreSQL wire protocol

Any Postgres client — `psql`, drivers, ORMs — can connect directly. See the
[README's "Connect with a Postgres client" section](../README.md#connect-with-a-postgres-client)
for the full walkthrough.

## Vector similarity search

`ORDER BY cosine(...) LIMIT k` over an `f32[]` column. See
[`GETTING_STARTED.md` step 4](GETTING_STARTED.md#4-run-a-full-relational-workload-insertselectupdatedelete)
for the runnable snippet.

## Run the fault simulator

Injects randomized WAL tail corruptions (truncation, byte flips, zeroing) and
asserts `StorageEngine::recover` always yields a prefix-consistent state.

```sh
cargo test -p tpt-archon-core faultsim
```

## Try it without installing anything

- Interactive SQL REPL in Docker — see the README's [Quick start](../README.md#quick-start).
- Browser playground (wasm) — see [`crates/out-archon-wasm/www/`](../crates/out-archon-wasm/www/) and that crate's README.
