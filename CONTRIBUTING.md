# Contributing to tpt-archon

Thanks for considering a contribution. This project is a multi-phase,
strictly-layered Rust workspace under active bootstrap — read
[`CLAUDE.md`](CLAUDE.md) for the architecture overview and
[`TODO.md`](TODO.md) for what's built vs. deferred before diving in.

## Before you start

- **Check the layering rule first.** Crates depend strictly downward:
  `tpt-archon-relational → tpt-archon-kernel → tpt-archon-bridge →
  tpt-archon-core`, with `out-archon-sql` depending only on
  `tpt-archon-relational` and `out-archon-verify` sitting off to the side.
  A PR that adds a reverse-direction dependency (e.g. `tpt-archon-core`
  reaching into `tpt-archon-bridge`) will be rejected regardless of how
  useful the shortcut looks — see `CLAUDE.md`'s "Workspace layout" section
  for the full picture.
- **Naming convention matters.** Crates published to crates.io are prefixed
  `tpt-archon-`; crates that are never published (demo tools, the
  verification harness) are prefixed `out-archon-`. Don't add a new crate
  without picking the right prefix.
- For anything beyond a small fix, consider opening an issue first to align
  on approach — especially for anything touching the capability system,
  WAL/crash-recovery format, or MVCC semantics, where correctness bugs are
  expensive to find later.

## Local setup

```
cargo build --workspace
cargo test --workspace
```

See the `Commands` section of `CLAUDE.md` for the full set (no_std check,
single-crate/single-test runs, formatting, lint, packaging dry-run).

## Before opening a PR

All of these are enforced in CI (`.github/workflows/ci.yml`) — running them
locally first saves a round trip:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p tpt-archon-core --no-default-features   # no_std check
```

If your change touches `crates/out-archon-verify` (the git-dependent
verification harness, excluded from the default workspace), also run:

```
cargo fmt -p out-archon-verify -- --check
cargo test -p out-archon-verify
```

from `crates/out-archon-verify/`.

## What to include in a PR

- Tests for new behavior or bug fixes — colocated `#[cfg(test)]` modules
  per the existing convention (see "Testing conventions" in `CLAUDE.md`).
- A short description of *why*, not just *what* — the codebase already
  documents intent heavily in `TODO.md`/ADRs (`docs/`); PRs that change
  behavior should update those if they affect what's tracked there.
- No new external dependencies in `tpt-archon-core` (it deliberately has
  zero, to stay the first crate publishable) without discussing it first.

## Reporting bugs / security issues

Use the issue templates. For anything you believe is a genuine security
vulnerability (not a documented known-limitation from `TODO.md`), please
open an issue rather than a public PR with exploit details, so it can be
triaged first.
