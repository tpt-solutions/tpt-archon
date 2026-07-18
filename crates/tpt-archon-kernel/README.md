# tpt-archon-kernel

Phase 2b of [tpt-archon](https://github.com/tpt-solutions/tpt-archon): a
capability-based microkernel with a unified page cache.

> **"Microkernel" means a user-space process model first.** Per `spec.txt`'s
> Risk 1 mitigation, the architecture is validated as a user-space process on a
> host OS before any bare-metal or hardware-driver work. The crate does not yet
> run on bare metal, and its scheduler is a cooperative user-space scheduler,
> not a real `io_uring`/preemptive one.

## Modules

- [`scheduler`](src/scheduler.rs) — a cooperative, round-robin async `Scheduler`
  running one `Task` per DB connection (not an OS process). It always makes
  progress while any task is runnable.
- [`ipc`](src/ipc.rs) — capability-bearing `Message` passing via a
  `MessageRouter`. A message is only delivered if the sender holds a write
  capability for the destination channel.
- [`memory`](src/memory.rs) — `UnifiedMemory`, where the kernel page cache *is*
  the database buffer pool: a single allocation shared through the bridge's
  `UnifiedPageCache` trait, with capability-checked access.

## Features

- `std` (default) — forwards to the lower crates' `std` features. Build with
  `--no-default-features` for `no_std`.

## Publishing note

The dependencies on `tpt-archon-core`/`tpt-archon-bridge` are path dependencies
during development. Switch them to version requirements before publishing.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
