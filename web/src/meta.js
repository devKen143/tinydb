export default {
  name: 'tinydb',
  stack: 'Rust → WebAssembly · B+tree · SQL',
  tagline: 'A relational database engine built from the bottom up: paged storage, a B+tree, a rollback journal, and a SQL parser, planner and executor.',
  repo: 'https://github.com/devKen143/tinydb',
  description:
    'Tables are B+trees of 4 KB pages keyed by rowid. Commits are atomic thanks to a rollback journal, so a crash mid-write is undone on the next open. On top sits a hand-written SQL front end with joins, grouping, aggregates, indexes, constraints and EXPLAIN. The console below is the real engine, compiled to WebAssembly with in-memory pages.',
  features: [
    'CREATE/DROP TABLE and INDEX, INSERT, SELECT, UPDATE, DELETE, BEGIN/COMMIT/ROLLBACK, EXPLAIN',
    'INNER and LEFT JOIN, WHERE, GROUP BY, HAVING, ORDER BY (aliases and positions), LIMIT/OFFSET, DISTINCT',
    'COUNT/SUM/AVG/MIN/MAX (with DISTINCT), UPPER, LOWER, LENGTH, ROUND, SUBSTR, COALESCE, LIKE, IN, BETWEEN, IS NULL',
    'INTEGER PRIMARY KEY as the rowid, UNIQUE and NOT NULL constraints, DEFAULT values, type coercion',
    'Planner: primary-key lookups and ranges, index equality and ranges, index nested-loop joins',
    'Three-valued NULL logic, statement-level atomicity, crash-safe commits',
  ],
  demoNote: 'A sample schema is preloaded. Pick an example or write your own; Ctrl+Enter runs. Try EXPLAIN before and after CREATE INDEX.',
  architecture: [
    { title: 'pager.rs', text: 'Page cache over a Storage trait (file or memory). Commit writes the original images to a journal, fsyncs, writes the new pages, fsyncs, deletes the journal.' },
    { title: 'btree.rs', text: 'Leaf and interior nodes each fill one page. Splits are byte-balanced; the root page id never changes, so the catalog never needs updating.' },
    { title: 'sql/', text: 'A lexer and recursive-descent parser produce a typed AST with precedence for OR, AND, NOT, comparisons, IN, BETWEEN, LIKE and arithmetic.' },
    { title: 'db.rs', text: 'Splits the WHERE into conjuncts to choose an access path, then runs join → filter → group → project → sort → limit. Indexes are maintained on every write.' },
  ],
  usage: `git clone https://github.com/devKen143/tinydb && cd tinydb
cargo run --release -- shop.db

tinydb> CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT, price REAL);
tinydb> INSERT INTO products (name, price) VALUES ('kettle', 25), ('toaster', 40);
tinydb> SELECT name FROM products WHERE price < 30;
tinydb> .schema
tinydb> .stats`,
}
