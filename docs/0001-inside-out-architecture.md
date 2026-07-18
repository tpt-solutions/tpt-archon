# ADR 0001: Build inside-out (storage → kernel → database)

## Status

Accepted.

## Context

Legacy stacks (Linux + PostgreSQL + SQLite) compose storage, OS, and database
as independently-designed layers glued together after the fact, which is
where the double-buffering and middleware-tax copies (disk → kernel page
cache → DB buffer pool → query executor → socket) come from — see `spec.txt`
§"Eliminating the Middleware Tax".

## Decision

Build the storage engine (`tpt-archon-core`) first, so its page model is
fixed. Build the kernel's memory manager (`tpt-archon-bridge` +
`tpt-archon-kernel`) around that page model, so kernel and storage can share
one physical page cache instead of copying between two. Build the relational
engine (`tpt-archon-relational`) last, as a user-space service on top of the
kernel, so it inherits zero-copy access to storage pages instead of
maintaining its own buffer pool.

This is why the Cargo workspace dependency graph is strictly one-directional
(`relational → kernel → bridge → core`) rather than each crate being
independently designed and integrated later.

## Consequences

- Phase 1 (`tpt-archon-core`) has no dependency on kernel or database
  concepts and can be developed, tested, and published to crates.io in
  isolation.
- Phases 2-3 cannot start meaningfully until Phase 1's page model
  (`page/` module) is stable, since the unified page cache trait in
  `tpt-archon-bridge` is defined in terms of it.
- Changing the on-disk page format after `tpt-archon-kernel` exists is a
  breaking change across three crates, not one — get the page model right
  in Phase 1.
