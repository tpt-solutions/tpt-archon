# ADR 0002: Zero-allocation primitives live in-crate, not in `tpt-zero-bytes`

## Status

Accepted.

## Context

`spec.txt` repeatedly refers to a `tpt-zero-bytes` crate for zero-allocation
parsing and I/O. No such crate exists anywhere in the TPT ecosystem — it was
never built. The storage engine still needs allocation-free page/WAL framing
and fixed-capacity buffers, and the SQL parser needs a zero-copy tokenizer.

## Decision

Implement the zero-allocation primitives directly in `tpt-archon-core`, in the
[`zerocopy`](../crates/tpt-archon-core/src/zerocopy.rs) module:

- `FixedBuf<N>` — a `const`-sized inline byte buffer with a tracked length.
- `Cursor` / `Reader` — little-endian, bounds-checked, `unsafe`-free
  encode/decode over borrowed slices.

The relational parser reuses the same borrowing discipline (a tokenizer that
borrows from the input string) rather than importing a second copy.

## Consequences

- The read/write hot path (page framing, WAL records) allocates nothing and
  pulls in no `serde`.
- There is exactly one place to look for these primitives; a contributor who
  "helpfully" adds a `tpt-zero-bytes` dependency should be pointed here. The
  crate-level docs and `AGENTS.md` both call this out.
- If a real `tpt-zero-bytes` is ever published, migrating is a
  find-and-replace against the `zerocopy` module's small surface, not a
  redesign.
