// Plain ES module, no bundler/npm step: `wasm-pack build --target web`
// (see ../README.md) emits `../pkg/out_archon_wasm.js` (default export is an
// async `init()` that loads and instantiates `out_archon_wasm_bg.wasm`) plus
// the named exports declared in `src/lib.rs` (`ArchonDb`, `init_panic_hook`).
// This file, `index.html`, and `../pkg/` are the three things
// `.github/workflows/wasm-demo.yml` copies onto GitHub Pages as-is.
import init, { ArchonDb, init_panic_hook } from "../pkg/out_archon_wasm.js";

const sqlEl = document.getElementById("sql");
const runBtn = document.getElementById("run");
const resetBtn = document.getElementById("reset");
const statusEl = document.getElementById("status");
const outputEl = document.getElementById("output");
const errorEl = document.getElementById("error");

let db = null;

function newDb() {
  db = new ArchonDb();
}

function renderResult(result) {
  errorEl.textContent = "";
  if (!result || !result.columns || result.columns.length === 0) {
    outputEl.innerHTML = "<p><em>OK (no result set)</em></p>";
    return;
  }
  const table = document.createElement("table");
  const thead = document.createElement("thead");
  const headRow = document.createElement("tr");
  for (const col of result.columns) {
    const th = document.createElement("th");
    th.textContent = col;
    headRow.appendChild(th);
  }
  thead.appendChild(headRow);
  table.appendChild(thead);

  const tbody = document.createElement("tbody");
  for (const row of result.rows) {
    const tr = document.createElement("tr");
    for (const cell of row) {
      const td = document.createElement("td");
      td.textContent = cell === null ? "NULL" : Array.isArray(cell) ? `[${cell.join(", ")}]` : String(cell);
      tr.appendChild(td);
    }
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);

  outputEl.innerHTML = "";
  outputEl.appendChild(table);
  outputEl.insertAdjacentHTML(
    "beforeend",
    `<p><small>${result.rows.length} row(s)</small></p>`,
  );
}

function runAll() {
  const text = sqlEl.value;
  // Split on statement-terminating `;` — a simple heuristic that doesn't
  // account for `;` inside a string literal; fine for a demo playground, not
  // a real SQL tokenizer (the actual tokenizer lives in
  // `tpt-archon-relational::parser` and only sees one statement at a time
  // here).
  const statements = text
    .split(";")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);

  errorEl.textContent = "";
  let lastResult = null;
  for (const stmt of statements) {
    try {
      const json = db.execute(stmt);
      lastResult = JSON.parse(json);
    } catch (err) {
      errorEl.textContent = `Error running: ${stmt}\n\n${err}`;
      renderResult(lastResult);
      return;
    }
  }
  renderResult(lastResult);
}

runBtn.addEventListener("click", runAll);
resetBtn.addEventListener("click", () => {
  newDb();
  outputEl.innerHTML = "";
  errorEl.textContent = "";
  statusEl.textContent = "Database reset.";
});

async function main() {
  await init();
  init_panic_hook();
  newDb();
  runBtn.disabled = false;
  statusEl.textContent = "Ready.";
}

main().catch((err) => {
  statusEl.textContent = "Failed to load WASM module.";
  errorEl.textContent = String(err);
});
