# tinydb

A small relational database engine written from scratch in Rust: paged storage, a B+tree, a rollback journal for atomic commits, and a SQL parser, planner and executor. The same engine compiles to WebAssembly for an in-browser SQL console (repo homepage).

```
$ cargo run --release -- shop.db
tinydb> CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT UNIQUE, price REAL);
tinydb> INSERT INTO products (name, price) VALUES ('kettle', 25), ('toaster', 40);
tinydb> SELECT name, price FROM products WHERE price < 30;
+--------+-------+
| name   | price |
+--------+-------+
| kettle |  25.0 |
+--------+-------+
1 row
```

## Architecture
| Layer | File | Notes |
|---|---|---|
| Storage | `pager.rs` | 4 KB pages, cache, `Storage` trait (file / memory), rollback journal with checksum |
| Index structure | `btree.rs` | B+tree keyed by i64 rowid, byte-balanced splits, stable root, chained leaves |
| Values | `value.rs` | INTEGER / REAL / TEXT / BOOLEAN / NULL, total ordering, row + schema encoding |
| SQL | `sql/` | lexer, AST, recursive-descent parser |
| Engine | `db.rs` | catalog, access-path planner, joins, aggregation, constraints, indexes, transactions |

**Commit protocol:** original images of dirty pages → journal → fsync → write pages → fsync → delete journal. A journal found at open time is rolled back, so a crash mid-commit leaves the previous state intact (tested).

## SQL support
- `CREATE TABLE [IF NOT EXISTS]`, `DROP TABLE [IF EXISTS]`, `CREATE [UNIQUE] INDEX`, `DROP INDEX`
- `INSERT ... VALUES (...), (...)`, `UPDATE ... SET ... WHERE`, `DELETE ... WHERE`
- `SELECT [DISTINCT] ... FROM t [alias] [[LEFT] JOIN u ON ...] WHERE ... GROUP BY ... HAVING ... ORDER BY ... LIMIT n OFFSET m`
- `BEGIN`, `COMMIT`, `ROLLBACK`, `EXPLAIN <stmt>`
- Aggregates `COUNT(*)`, `COUNT/SUM/AVG/MIN/MAX([DISTINCT] x)`; functions `UPPER LOWER LENGTH TRIM ABS ROUND SUBSTR COALESCE IFNULL`
- Operators `= <> != < <= > >= AND OR NOT + - * / % ||`, `LIKE`, `IN`, `BETWEEN`, `IS [NOT] NULL`
- Constraints: `INTEGER PRIMARY KEY` (rowid alias), `PRIMARY KEY`, `UNIQUE`, `NOT NULL`, `DEFAULT`

## Limitations
- Rows are limited to ~2 KB (no overflow pages); deleted space is not reused (no VACUUM / freelist)
- Secondary indexes are kept in memory and rebuilt when the database is opened
- Single writer, no concurrent connections

## Shell
`tinydb [file.db]` (omit the file or use `:memory:` for an in-memory database). Dot-commands: `.tables`, `.schema [table]`, `.stats`, `.quit`.

## Tests
`cargo test` — B+tree stress (5,000 scrambled inserts with splits, deletes), SQL semantics, joins, aggregates, constraints, transactions, planner choices, persistence and crash recovery.

## License
MIT
