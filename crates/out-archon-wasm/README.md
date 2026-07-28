# out-archon-wasm

A browser playground for `tpt-archon-relational`'s SQL engine: `wasm-bindgen`
glue (`src/lib.rs`, the `ArchonDb` type) plus a static page (`www/`) that runs
SQL entirely client-side, no server involved.

Not published (`publish = false`) — a dev/demo tool, not one of the layered
architecture crates. See the root `CLAUDE.md` for the `tpt-archon-`/
`out-archon-` naming convention.

## What this is (and isn't)

- **IS:** the same `Database::empty()` + `parse_statement` + `execute` flow
  the `archon-sql` REPL (`crates/out-archon-sql`) uses, wrapped behind a
  `wasm-bindgen` API (`ArchonDb::new()` / `ArchonDb::execute(sql)`) so it runs
  in a browser tab instead of a terminal. CI's `wasm` job has proven
  `tpt-archon-relational` compiles for `wasm32-unknown-unknown` for a while;
  this crate is what actually exercises that build in a browser.
- **NOT (yet):** persistent storage (the database lives only in WASM linear
  memory for the tab's lifetime — reload the page and it's gone), multiple
  databases per page, or a bundler/npm-based build. `www/index.js` is a plain
  ES module with a relative import; no `npm install` is needed to try the
  page locally.

## Build it locally

Install the `wasm32-unknown-unknown` target and `wasm-pack` once:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

Then, from this directory:

```sh
wasm-pack build --target web --out-dir pkg
```

This emits `pkg/out_archon_wasm.js` (the JS glue + a default `init()`
function) and `pkg/out_archon_wasm_bg.wasm` (the compiled module),
which `www/index.js` imports directly via a relative path — no bundler step.

If you don't have (or don't want) `wasm-pack`, `wasm-bindgen-cli` works too
once you've built the `cdylib` yourself:

```sh
cargo build -p out-archon-wasm --target wasm32-unknown-unknown --release
wasm-bindgen target/wasm32-unknown-unknown/release/out_archon_wasm.wasm \
  --out-dir pkg --target web
```

## Try the page

Browsers block `fetch()` of `file://` URLs (which is how the generated glue
loads the `.wasm` binary), so serve `www/` and `pkg/` over plain HTTP. Any
static file server works, e.g.:

```sh
# from crates/out-archon-wasm/, after the wasm-pack build above:
python3 -m http.server 8000
# then open http://localhost:8000/www/
```

(`pkg/` needs to be a sibling of `www/`, as it is right after the
`wasm-pack build --out-dir pkg` command above, since `index.js` imports
`../pkg/out_archon_wasm.js`.)

Type SQL into the textarea and click **Run**:

```sql
CREATE TABLE users (id INT, name TEXT, age INT);
INSERT INTO users (id, name, age) VALUES (1, 'alice', 30);
INSERT INTO users (id, name, age) VALUES (2, 'bob', 25);
SELECT name, age FROM users WHERE age >= 25 ORDER BY age;
```

## CI / deployment

`.github/workflows/wasm-demo.yml` builds this crate with `wasm-pack` on push
to the default branch and deploys `www/` + `pkg/` to GitHub Pages via
`actions/deploy-pages`. See that workflow for the exact steps; it's the
"real" version of the local build above, run automatically.

## Verifying without a browser

```sh
cargo check -p out-archon-wasm --target wasm32-unknown-unknown
```

confirms the crate (and its `tpt-archon-relational` dependency) compiles for
the actual target this playground ships to. `cargo build --workspace` /
`cargo test --workspace` from the repo root also cover this crate on the host
target (`wasm-bindgen` compiles fine off-target; it just doesn't generate
anything browser-useful there), which is why this crate is an ordinary
workspace member rather than excluded like `out-archon-verify`.
