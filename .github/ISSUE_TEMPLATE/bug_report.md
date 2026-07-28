---
name: Bug report
about: Report incorrect behavior in a shippable crate
title: "[bug] "
labels: bug
---

**Which crate?** (`tpt-archon-core` / `tpt-archon-bridge` / `tpt-archon-kernel` / `tpt-archon-relational` / `out-archon-sql` / `out-archon-verify`)

**What happened?**
A clear description of the incorrect behavior.

**Expected behavior**
What you expected instead.

**Repro**
Minimal code, SQL, or CLI invocation that reproduces the issue. For
`archon-sql`, include the exact statements you ran.

```
// paste here
```

**Environment**
- `cargo --version` / Rust toolchain:
- OS:
- Crate version / commit:

**Before filing**
- [ ] I checked [`TODO.md`](../../TODO.md) — this isn't a documented,
      already-tracked known limitation (e.g. `io_uring`, real `mmap`, GPU
      aggregation/UDFs, unbounded `LIMIT`).
- [ ] If this is a security vulnerability, I have not included exploit
      details in a public issue (see [`CONTRIBUTING.md`](../../CONTRIBUTING.md)).
