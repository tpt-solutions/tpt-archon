# PostgreSQL Compatibility — Divergence Catalog

This document is the honest, machine-tracked answer to `spec.txt`'s claim that
Archon is a *"PostgreSQL-compatible SQL dialect"* and a *"drop-in replacement for
most PostgreSQL workloads"* (see `spec.txt:134`, `spec.txt:270`). It is **generated
from the `divergent/` corpus** under
`crates/tpt-archon-relational/tests/slt/divergent/` — every entry below is a
literal divergence encoded as an `slt` directive that `cargo test -p
tpt-archon-relational slt_corpus` asserts Archon keeps producing. It is **not**
hand-maintained prose that can silently drift: when a divergence is fixed, the
underlying test moves into `supported/` and must be removed from here (see the
"move-don't-edit-in-place" rule in `divergent/known_bugs.slt`).

**How the comparison actually works (Phase 8 / Track C):**
- `tests/slt.rs` runs `supported/*.slt` **and** `divergent/*.slt` against
  `Database` directly as a normal, offline `cargo test --workspace` integration
  test. `divergent/` encodes Archon's *own* (divergent) expected behavior, so it
  only proves Archon stays stable — it does not compare against Postgres.
- `crates/out-archon-pgcompat` is the real-Postgres oracle: it runs
  `supported/*.slt` against a live PostgreSQL (`pgvector/pgvector:pg16` per
  `docker-compose.yml`) and asserts Postgres agrees with the corpus's expected
  rows. It **skips `divergent/` entirely** (by definition Postgres is not
  expected to match a documented Archon divergence). The `pg-compat` CI job runs
  it; without a live Postgres it exits successfully and does nothing.

So: `supported/` = behavior validated against *both* engines; `divergent/` =
behavior asserted to differ from Postgres, cataloged below.

---

## Verdict on the `spec.txt` claim

The "PostgreSQL-compatible / drop-in replacement" wording is **currently false**
as an unconditional claim. The dialect is a substantial subset: one join-family
grammar, a handful of column types, no set operations beyond `UNION`/`INTERSECT`/
`EXCEPT`, no window functions coverage gap relative to Postgres's full `OVER`
surface, no `RANGE`/`GROUPS` frame variety, no wire-protocol parity for every
driver feature, and no `pg_catalog` emulation. The items below are the
*known, named* divergences. Closing the remaining grammar/feature gaps is tracked
in the wider Phase 8 roadmap (Track A), not here. This document records what is
**divergent today**, not the full forward roadmap.

---

## Catalog of divergences

### D1 — Qualified JOIN `ON` column silently falls back on an unknown qualifier

- **Source:** `tests/slt/divergent/known_bugs.slt` (fact #2, narrowed)
- **Category:** Known bug (engine defect, not a stylistic gap)
- **Severity:** Medium — wrong results are possible, silently
- **Status:** **Fixed** (2026-08-05) — regression test at
  `tests/slt/supported/join_qualified_column.slt`

A `JOIN ... ON` clause that names a *real* table in the join resolves via exact
`table.col` matching (see `supported/joins.slt` and `run_select_scoped`'s
`on_cols` in `database/select.rs`). A qualifier naming a table **not** in the
join now errors instead of silently matching a same-suffix column:
`executor::find_value` only applies the trailing-segment fallback when the
qualifier names a real in-scope table/alias (threaded through `eval_where` and
the `JOIN ON` evaluation in `database/select.rs`).

```sql
CREATE TABLE t3 (v INT);
CREATE TABLE t4 (v INT);
INSERT INTO t3 (v) VALUES (1);
INSERT INTO t4 (v) VALUES (1);
-- 'bogus' is not a table in this query; Archon now rejects it as an unknown column.
SELECT v FROM t3 JOIN t4 ON bogus.v = t4.v;  -- ERROR: unknown column: bogus.v
```

---

### D2 — `AVG(int)` returns `float`, not Postgres's `numeric`

- **Source:** `tests/slt/divergent/aggregates.slt` (fact #5, narrowed)
- **Category:** Type-system divergence (value is now correct; presentation/type is not)
- **Severity:** Low-to-medium — correct value, but a driver comparing exact text
  or relying on `numeric` typing will see a mismatch
- **Status:** Open (the integer-division half of fact #5 is **fixed** and moved
  to `supported/aggregates.slt`; only the `numeric` text form remains)

`executor::eval_aggregate`'s `AVG` now divides as a float (`AVG(1,2) = 1.5`, not
`1` via truncating `i64` division — that part is fixed). What still diverges:
Postgres's `AVG(int)` always returns `numeric` (an exact decimal type), whose
text form keeps the full computed scale — e.g. `1.5000000000000000` — whereas
`Value::Float` renders `1.5`.

```sql
CREATE TABLE nums (n INT);
INSERT INTO nums (n) VALUES (1), (2);
SELECT AVG(n) AS avg_n FROM nums;
```

- **Archon:** `1.5`
- **Postgres:** `1.5000000000000000`

`SUM` does **not** have this divergence (Postgres's `SUM(int)` returns `numeric`
too, but Archon's integer `SUM` matches Postgres's *value* and the simpler text
form is accepted for the corpus's purpose). Only `AVG` is documented here because
Postgres's always-`numeric` typing rule uniquely forces a scale Postgres itself
does not apply to the `SUM` text form the same way.

---

### D3 — `HAVING` accepts output-alias references Postgres rejects

- **Source:** `tests/slt/divergent/aggregates.slt` (additional divergence found
  while building the corpus; beyond fact #5)
- **Category:** Deliberate superset (Archon accepts **both** spellings)
- **Severity:** None for correctness — this is strictly more permissive
- **Status:** Intentional, kept by decision (2026-08-04)

Per standard SQL logical query-processing order, `HAVING` is evaluated **before**
the `SELECT` list's output aliases exist, so Postgres rejects `HAVING cnt >= 2`
when `cnt` is a `SELECT`-list aggregate alias — you must write the raw aggregate
(`HAVING COUNT(*) >= 2`). Archon parses `HAVING` as a predicate over the `GROUP
BY`'s *output* row, so it accepts **both** the alias form and the raw form.

```sql
CREATE TABLE div_employees (dept TEXT, salary INT);
INSERT INTO div_employees (dept, salary) VALUES ('eng',100),('eng',200),('sales',50);

-- Archon: alias form (Postgres rejects this with "column \"cnt\" does not exist")
SELECT dept, COUNT(*) AS cnt FROM div_employees GROUP BY dept HAVING cnt >= 2;
-- Archon: raw form (what Postgres requires)
SELECT dept, COUNT(*) AS cnt FROM div_employees GROUP BY dept HAVING COUNT(*) >= 2;
```

- **Archon:** both return `eng 2`.
- **Postgres:** first errors (`column "cnt" does not exist`); second returns
  `eng 2`.

**Decision:** keep Archon's behavior. Removing the alias form would be a usability
regression with no correctness upside (Archon matches Postgres's *required* form
and additionally accepts the convenient one). Revisit only if byte-identical
Postgres *error* behavior becomes a hard requirement for Track C. This is why no
single `HAVING` spelling can be placed in `supported/`: Postgres rejects one of
the two forms Archon accepts.

---

### D4 — `FROM` mandatory for any non-literal `SELECT` list

- **Source:** `tests/slt/divergent/grammar_gaps.slt` (fact #3, partially resolved)
- **Category:** Grammar gap (scope, being closed by Track A)
- **Severity:** Medium for ad-hoc/tooling SQL
- **Status:** Partially open

Postgres supports `SELECT <expr>` with no `FROM` (e.g. `SELECT 1`, `SELECT
now()`). A `Statement::SelectLiteral` path now handles the common case — a
comma-separated list of pure literals (optionally aliased), nothing after — run
directly as a one-row result (see `supported/ddl.slt` for passing examples).
Fixing this also removed a real live bug in `out-archon-pgwire`'s `compat.rs`
health-check shim (infinite recursion / never calling the real parser).

What is **not** resolved:

```sql
-- A bare column reference is not a literal, so Archon still errors here:
SELECT v            -- Archon: ParseError("expected FROM")
                    -- Postgres: ERROR: column "v" does not exist
```

Note the *message* even differs: Archon reports a parse error ("expected FROM"),
Postgres reports an unknown-column error. General scalar expressions in a real
table's `SELECT` list remain a distinct gap (the normal `SELECT`-list grammar
still accepts column names/aggregates only).

---

### D5 — `LEFT`/`RIGHT`/`FULL`/`CROSS JOIN` (roadmap fact #4) and multi-row
`INSERT` (fact #7)

- **Source:** `tests/slt/divergent/grammar_gaps.slt` (facts #4 and #7)
- **Category:** Resolved scope gaps (kept here **annotated "(resolved)"** rather
  than moved — this file's own convention, distinct from `known_bugs.slt`'s
  move rule)
- **Severity:** None (resolved)
- **Status:** **Resolved** (Track A3 / multi-row `INSERT`)

These are listed in `divergent/` only as historical record; the behaviors now
match Postgres and pass:

```sql
CREATE TABLE g1 (v INT);
CREATE TABLE g2 (v INT);
INSERT INTO g1 (v) VALUES (1);
INSERT INTO g2 (v) VALUES (1);
SELECT v FROM g1 LEFT JOIN g2 ON v = v;   -- 1
SELECT v FROM g1 CROSS JOIN g2;           -- 1

-- Multi-row INSERT now exists (fact #7 resolved):
INSERT INTO g1 (v) VALUES (2), (3);
SELECT v FROM g1 ORDER BY v;              -- 1 / 2 / 3
```

They are retained in `divergent/grammar_gaps.slt` with an explicit `(resolved)`
annotation so the corpus records the gap-was-here history, not to assert current
divergence.

---

## Resolved-and-moved (for completeness)

The following were previously tracked as divergences and are now **fixed and
moved into `supported/`** (per `known_bugs.slt`'s header note):

- **Fact #1:** `CREATE TABLE` no longer lets a user column named `id` silently
  collide with the implicit row-id column — it is now rejected outright. Test
  moved to `supported/ddl.slt`.
- **Fact #6:** the lexer now honors SQL-standard doubled-quote escaping. Test
  moved to `supported/ddl.slt`.

---

## What this catalog does **not** cover

This file is the divergence catalog only. It does **not** enumerate:
- forward roadmap items (full `OVER` frame variety, `pg_catalog` emulation,
  extended-protocol parameter binding, SCRAM, `COPY`, TLS — see Phase 8 Track B
  stretch goals in `TODO.md`);
- the resolved `supported/` behaviors (validated against real Postgres by
  `out-archon-pgcompat`);
- formal-verification claims (WAL replay, MVCC serializability, B-Link invariants
  — see `formal-proofs/` and `crates/out-archon-verify`).

When any `divergent/` test is fixed, move it to `supported/` and delete its entry
above. The corpus test (`slt_corpus`) is the source of truth; this document is its
human-readable mirror.
