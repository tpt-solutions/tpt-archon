# Contributing to tpt-archon

Thanks for your interest in tpt-archon. This project is a multi-phase,
strictly-layered Rust workspace under active bootstrap — read
[`CLAUDE.md`](CLAUDE.md) for the architecture overview and
[`TODO.md`](TODO.md) for what's built vs. deferred before diving in.

## Contributions go through issues, not unsolicited PRs

This project does not accept unsolicited pull requests. If you've found a
bug, want to propose a feature, or spot a gap in the docs, please
[open an issue](.github/ISSUE_TEMPLATE) first using the bug report or feature
request template. That's the way to get something fixed or built — a
maintainer will follow up on the issue, and any resulting code change is
scoped and merged from there.

Before filing:
- **Check the layering rule.** Crates depend strictly downward:
  `tpt-archon-relational → tpt-archon-kernel → tpt-archon-bridge →
  tpt-archon-core`, with `out-archon-sql` depending only on
  `tpt-archon-relational` and `out-archon-verify` sitting off to the side.
  A proposal that needs a reverse-direction dependency (e.g. `tpt-archon-core`
  reaching into `tpt-archon-bridge`) will need a different shape — see
  `CLAUDE.md`'s "Workspace layout" section for the full picture.
- **Check `TODO.md`.** Both issue templates ask you to confirm the gap isn't
  already a documented, deliberately-deferred milestone (e.g. `io_uring`,
  real `mmap`, GPU aggregation/UDFs).
- **Naming convention matters**, if your proposal involves a new crate:
  crates published to crates.io are prefixed `tpt-archon-`; crates that are
  never published (demo tools, the verification harness) are prefixed
  `out-archon-` instead.

## Reproducing a bug locally

```sh
cargo build --workspace
cargo test --workspace
```

If you have [`just`](https://github.com/casey/just) installed, `just check`
runs the same gate CI enforces (`.github/workflows/ci.yml`) in one command —
format check, clippy with warnings denied, the full test suite, and the
`no_std` build check for `tpt-archon-core`. Equivalent explicit commands (see
also the `Commands` section of `CLAUDE.md`):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p tpt-archon-core --no-default-features   # no_std check
```

If your report touches `crates/out-archon-verify` (the git-dependent
verification harness, excluded from the default workspace), also run
`just verify`, or `cargo test -p out-archon-verify` from
`crates/out-archon-verify/`.

Including the exact failing command output, or a minimal repro (code, SQL, or
CLI invocation), in your issue makes it far more actionable — see the bug
report template for the shape.

## Reporting bugs / security issues

Use the issue templates. For anything you believe is a genuine security
vulnerability (not a documented known-limitation from `TODO.md`), please
open an issue rather than a public PR with exploit details, so it can be
triaged first.
