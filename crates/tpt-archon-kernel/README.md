# tpt-archon-kernel

Phase 2b of [tpt-archon](https://github.com/tpt-solutions/tpt-archon): a
capability-based microkernel with a unified page cache.

> **"Microkernel" means a user-space process model first.** Per `spec.txt`'s
> Risk 1 mitigation, the architecture is validated as a user-space process on a
> host OS before any bare-metal or hardware-driver work. The crate does not yet
> run on bare metal. Its scheduler is a cooperative, hand-rolled round-robin
> scheduler (not preemptive), but I/O tasks can now be backed by a real Linux
> `io_uring` reactor (see the `io-uring-backend` feature below) rather than a
> synchronous stand-in.

## Modules

- [`scheduler`](src/scheduler.rs) — a cooperative, round-robin async `Scheduler`
  running one `Task` per DB connection (not an OS process). It always makes
  progress while any task is runnable.
- [`ipc`](src/ipc.rs) — capability-bearing `Message` passing via a
  `MessageRouter`. A message is only delivered if the sender holds a write
  capability for the destination channel.
- [`memory`](src/memory.rs) — `UnifiedMemory`, where the kernel page cache *is*
  the database buffer pool: a single allocation shared through the bridge's
  `UnifiedPageCache` trait, with capability-checked access. Also exposes
  `map_read_zero_copy` (`mmap` feature) over the bridge's `MmapPageSource`
  trait — real OS-mapped, read-only shared memory, not just an in-process
  reference.
- [`io_uring_backend`](src/io_uring_backend.rs) (Linux only, `io-uring-backend`
  feature) — a `Reactor` wrapping a real `io_uring` submission/completion ring,
  plus `IoReadTask`/`IoWriteTask`, ordinary `Task`s that submit a read/write and
  yield `Pending` until the kernel reports completion. Plugs into `Scheduler`
  unchanged: a pending I/O task stays in the same ready queue as any other
  pending task, so the scheduler's deadlock-freedom argument
  (`formal-proofs/scheduler.telos`) still covers it.

## Features

- `std` (default) — forwards to the lower crates' `std` features. Build with
  `--no-default-features` for `no_std`.
- `io-uring-backend` — enables the real `io_uring` reactor (implies `std`).
  Linux only; the `io-uring` dependency doesn't build on other platforms, so
  this is off by default and meant to be enabled explicitly (e.g. a Linux-only
  CI job or a Linux deployment target).
- `mmap` — enables real, read-only, OS-`mmap`-backed zero-copy page access
  (implies `std`). Cross-platform (`memmap2`: `mmap(2)` on Unix,
  `CreateFileMappingW` on Windows) — unlike `io-uring-backend`, no target
  gating is needed and this is exercised on every OS in CI.

## Publishing note

The dependencies on `tpt-archon-core`/`tpt-archon-bridge` are path dependencies
during development. Switch them to version requirements before publishing.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
