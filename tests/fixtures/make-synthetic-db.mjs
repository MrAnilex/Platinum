// Builds tests/fixtures/synthetic.db, the database the unit tests of src/sqlite.rs read.
// Every value is made up here: nothing comes from any vendor's database.
//
// It covers what Platinum's reader must handle in GG's preset database: 4096-byte pages, UTF-8,
// rollback journal, a schema that spans several pages (so page 1 is an interior page), a table
// deep enough for an interior page, rows that spill to one and to several overflow pages, short
// records written before ALTER TABLE ADD COLUMN, a quoted table name with a table-level
// PRIMARY KEY, an INTEGER PRIMARY KEY (rowid alias), REAL, BLOB, NULL and negative values, and a
// WITHOUT ROWID table the reader must refuse.
//
// Usage, from platinum/: node tests/fixtures/make-synthetic-db.mjs tests/fixtures/synthetic.db

import { DatabaseSync } from 'node:sqlite';
import { existsSync, rmSync } from 'node:fs';

const out = process.argv[2];
if (!out) throw new Error('usage: node make-synthetic-db.mjs <output.db>');
for (const suffix of ['', '-journal', '-wal', '-shm']) if (existsSync(out + suffix)) rmSync(out + suffix);

// Same formula as `pattern` in src/sqlite.rs: an 'é' every 100th character, so that page
// boundaries fall inside two-byte characters.
const ALPHABET = 'abcdefghijklmnopqrstuvwxyz0123456789';
const pattern = (length, seed) =>
  Array.from({ length }, (_, k) => (k % 100 === 99 ? 'é' : ALPHABET[(k * 7 + seed) % 36])).join('');

const db = new DatabaseSync(out);
db.exec('PRAGMA page_size = 4096; PRAGMA journal_mode = DELETE; PRAGMA encoding = "UTF-8";');

db.exec(`CREATE TABLE items (id INTEGER PRIMARY KEY, label TEXT NOT NULL UNIQUE, amount INTEGER NOT NULL,
  ratio REAL, flag BOOL NOT NULL DEFAULT false, raw BLOB)`);
const insertItem = db.prepare('INSERT INTO items (id, label, amount, ratio, flag, raw) VALUES (?, ?, ?, ?, ?, ?)');
for (let i = 1; i <= 150; i++) {
  const label = `item-${i}-` + 'x'.repeat(40) + (i % 10 === 0 ? ' é™ō🎵' : '');
  const amount = i % 50 === 0 ? 2 ** 40 + i : i % 2 ? -i * i : i * i;
  const raw = i % 7 === 0 ? new Uint8Array([i, i + 1, i + 2]) : null;
  insertItem.run(i, label, amount, i / 4, i % 3 === 0 ? 1 : 0, raw);
}
// Rows written before these columns existed keep fewer columns: the reader fills in the defaults.
db.exec(`ALTER TABLE items ADD COLUMN tag TEXT NOT NULL DEFAULT 'none';
  ALTER TABLE items ADD COLUMN rank INTEGER DEFAULT -1;
  ALTER TABLE items ADD COLUMN note TEXT DEFAULT NULL;
  ALTER TABLE items ADD COLUMN score REAL DEFAULT 2.5;`);
const insertFull = db.prepare('INSERT INTO items (id, label, amount, ratio, flag, raw, tag, rank, note, score) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)');
for (let i = 151; i <= 160; i++) insertFull.run(i, `item-${i}`, i, 0.5, 1, null, `tag-${i}`, i - 150, `note ${i}`, -1.25);
db.exec("UPDATE items SET tag = 'updated' WHERE id = 5");

db.exec(`CREATE TABLE "documents" (id TEXT NOT NULL UNIQUE, name TEXT NOT NULL, kind INTEGER NOT NULL,
  body TEXT NOT NULL, PRIMARY KEY(id))`);
const insertDoc = db.prepare('INSERT INTO documents (id, name, kind, body) VALUES (?, ?, ?, ?)');
insertDoc.run('doc-1', 'short', 1, pattern(50, 1));
insertDoc.run('doc-2', 'one overflow page', 2, pattern(5000, 2));
insertDoc.run('doc-3', 'several overflow pages', 3, pattern(20000, 3));
db.exec("ALTER TABLE documents ADD COLUMN extra TEXT NOT NULL DEFAULT ''");
db.prepare('INSERT INTO documents (id, name, kind, body, extra) VALUES (?, ?, ?, ?, ?)').run('doc-4', 'after alter', 4, pattern(300, 4), 'x');

db.exec(`CREATE TABLE pairs (slot INTEGER NOT NULL UNIQUE, doc TEXT NOT NULL UNIQUE,
  FOREIGN KEY(doc) REFERENCES "documents"(id))`);
db.exec("INSERT INTO pairs VALUES (1, 'doc-3'), (2, 'doc-1'), (3, 'doc-4')");

db.exec('CREATE TABLE empty_one (a TEXT)');
db.exec('CREATE TABLE no_rowid (k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID');
db.exec("INSERT INTO no_rowid VALUES ('a', 1)");

// Views take no page of their own, only schema rows: thirty long ones push the schema past page 1.
for (let v = 0; v < 30; v++) {
  db.exec(`CREATE VIEW padding_view_${v} AS SELECT id, label, '${pattern(300, v).replaceAll("'", "''")}' AS pad FROM items WHERE amount > ${v}`);
}

db.close();
console.log(`wrote ${out}`);
