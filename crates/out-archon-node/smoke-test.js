// Smoke test for the archon-node native module -- exercises CREATE TABLE,
// INSERT, SELECT ... WHERE ... ORDER BY, and the vector-search path
// (ORDER BY cosine(...) LIMIT k). Run after `npm run build`:
//
//   node smoke-test.js
//
// Exits non-zero (via the assert throw) on any mismatch.

const assert = require("node:assert");
const { Database } = require("./index.js");

const db = new Database();

// --- basic relational flow (mirrors the root README's Quick Start) -------

db.execute("CREATE TABLE users (name TEXT, age INT)");
db.execute("INSERT INTO users (name, age) VALUES ('alice', 30)");
db.execute("INSERT INTO users (name, age) VALUES ('bob', 25)");
db.execute("INSERT INTO users (name, age) VALUES ('carol', 40)");

const rows = db.execute(
  "SELECT name, age FROM users WHERE age >= 25 ORDER BY age"
);
assert.deepStrictEqual(rows, [
  { name: "bob", age: 25 },
  { name: "alice", age: 30 },
  { name: "carol", age: 40 },
]);
console.log("relational CRUD:", JSON.stringify(rows));

// --- vector column + ORDER BY cosine(...) LIMIT k (RAG-style top-k) ------

db.execute("CREATE TABLE docs (label TEXT, embedding VECTOR[3])");
db.execute(
  "INSERT INTO docs (label, embedding) VALUES ('close', [1.0, 0.0, 0.0])"
);
db.execute(
  "INSERT INTO docs (label, embedding) VALUES ('far', [0.0, 1.0, 0.0])"
);

const nearest = db.execute(
  "SELECT label FROM docs ORDER BY cosine(embedding, ?) LIMIT 1",
  [[1.0, 0.0, 0.0]]
);
assert.deepStrictEqual(nearest, [{ label: "close" }]);
console.log("vector top-k:", JSON.stringify(nearest));

// --- error path -----------------------------------------------------------

assert.throws(() => db.execute("SELECT * FROM does_not_exist"));
console.log("error handling: OK");

console.log("\nAll smoke tests passed.");
