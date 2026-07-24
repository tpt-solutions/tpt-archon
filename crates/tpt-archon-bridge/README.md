# tpt-archon-bridge

Phase 2a of [tpt-archon](https://github.com/tpt-solutions/tpt-archon):
zero-copy IPC and unified memory management gluing the
[`tpt-archon-core`](../tpt-archon-core) storage engine to the kernel.

## Modules

- [`capability`](src/capability.rs) — strongly-typed, unforgeable, revocable
  `Capability` tokens minted by a `CapabilityIssuer`. A capability grants a
  `Right` (`Read`/`Write`/`ReadWrite`) over a `Resource` (a page or a channel).
  Tokens cannot be fabricated from raw integers, and can be revoked.
- [`page_cache`](src/page_cache.rs) — the `UnifiedPageCache` trait that lets the
  kernel map storage pages directly into the database's address space with no
  double-buffering, plus `CorePageCache`, which adapts the core buffer pool to
  it. A page written via `tpt-archon-core` is visible through the bridge with
  no copy (see the integration test in that module).
- [`grant`](src/grant.rs) — `CapabilityGrant`, a thin safe layer over
  `UnifiedPageCache` that bundles the capability check and the page borrow
  into one call, returning a `MemoryView`/`MemoryViewMut` instead of a raw
  page reference. No new unsafe code: it's backed by the same safe borrows
  `map_read`/`map_write` already return.

## Features

- `std` (default) — forwards to `tpt-archon-core/std`. Build with
  `--no-default-features` for `no_std`.

## Publishing note

The dependency on `tpt-archon-core` is a path dependency during development.
Switch it to a version requirement before publishing (see the repo-root
`TODO.md`, Phase 2a crates.io readiness).

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
