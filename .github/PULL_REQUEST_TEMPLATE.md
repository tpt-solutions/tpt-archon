## What & why

<!-- What does this change, and what problem does it solve? -->

## Checklist

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo build -p tpt-archon-core --no-default-features` (if `tpt-archon-core` touched)
- [ ] No reverse-direction dependency introduced (see `CLAUDE.md`'s dependency graph)
- [ ] Tests added/updated for the behavior change
- [ ] `TODO.md` / relevant ADR updated if this closes or changes a tracked item

## Notes for reviewers

<!-- Anything non-obvious: tradeoffs made, follow-ups intentionally left out, etc. -->
