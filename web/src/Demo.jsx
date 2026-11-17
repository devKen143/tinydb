import { useEffect, useRef, useState } from 'react'

const SEED = `CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, city TEXT, email TEXT UNIQUE);
CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, item TEXT, amount REAL, placed TEXT);
INSERT INTO customers (name, city, email) VALUES
  ('Ada Lovelace', 'London', 'ada@example.com'), ('Grace Hopper', 'New York', 'grace@example.com'),
  ('Alan Turing', 'London', 'alan@example.com'), ('Katherine Johnson', 'Hampton', NULL),
  ('Linus Torvalds', 'Portland', 'linus@example.com');
INSERT INTO orders (customer_id, item, amount, placed) VALUES
  (1, 'notebook', 12.5, '2026-01-04'), (1, 'pen', 3, '2026-01-05'), (2, 'compiler manual', 48, '2026-02-11'),
  (3, 'chess set', 35, '2026-02-12'), (3, 'notebook', 12.5, '2026-03-01'), (5, 'keyboard', 89.99, '2026-03-02'),
  (2, 'pen', 3, '2026-03-09'), (1, 'lamp', 27, '2026-04-20');`

const EXAMPLES = {
  'Join + aggregate': `SELECT c.name, COUNT(o.id) AS orders, SUM(o.amount) AS total
FROM customers c LEFT JOIN orders o ON o.customer_id = c.id
GROUP BY c.name
ORDER BY total DESC;`,
  'Filter & sort': `SELECT name, city FROM customers
WHERE city IN ('London', 'Portland') OR email IS NULL
ORDER BY city, name;`,
  'GROUP BY / HAVING': `SELECT item, COUNT(*) AS n, ROUND(AVG(amount), 2) AS avg_amount
FROM orders GROUP BY item HAVING COUNT(*) > 1 ORDER BY n DESC;`,
  'EXPLAIN + index': `EXPLAIN SELECT * FROM orders WHERE item = 'pen';
CREATE INDEX orders_item ON orders (item);
EXPLAIN SELECT * FROM orders WHERE item = 'pen';
EXPLAIN SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id;`,
  'Transaction rollback': `BEGIN;
DELETE FROM orders;
SELECT COUNT(*) AS during_txn FROM orders;
ROLLBACK;
SELECT COUNT(*) AS after_rollback FROM orders;`,
  'Constraints': `INSERT INTO customers (name, email) VALUES ('Impostor', 'ada@example.com');`,
  'Schema & stats': `.tables
.schema
.stats`,
}

export default function Demo() {
  const [sql, setSql] = useState(EXAMPLES['Join + aggregate'])
  const [out, setOut] = useState('Loading tinydb.wasm…')
  const [ok, setOk] = useState(true)
  const [ms, setMs] = useState(null)
  const wasm = useRef(null)

  const exec = (src) => {
    const e = wasm.current
    const b = new TextEncoder().encode(src)
    const p = e.alloc(b.length)
    new Uint8Array(e.memory.buffer, p, b.length).set(b)
    const code = e.run(p, b.length)
    e.dealloc(p, b.length)
    return [code === 0, new TextDecoder().decode(new Uint8Array(e.memory.buffer, e.output_ptr(), e.output_len()))]
  }

  useEffect(() => {
    WebAssembly.instantiateStreaming(fetch('/tinydb.wasm'))
      .then(({ instance }) => { wasm.current = instance.exports; exec(SEED); setOut('Sample tables customers and orders loaded. Press Run.') })
      .catch((e) => setOut(`failed to load wasm: ${e}`))
  }, [])

  function run() {
    if (!wasm.current) return
    const t0 = performance.now()
    const [good, text] = exec(sql)
    setMs((performance.now() - t0).toFixed(1))
    setOk(good)
    setOut(text)
  }
  function reset() {
    wasm.current.reset(); exec(SEED); setOut('Database reset to the sample data.'); setMs(null); setOk(true)
  }

  return (
    <div>
      <div className="row">
        <select onChange={(e) => setSql(EXAMPLES[e.target.value])} aria-label="example query">
          {Object.keys(EXAMPLES).map((k) => <option key={k}>{k}</option>)}
        </select>
        <button className="primary" onClick={run}>Run ▶</button>
        <button onClick={reset}>Reset data</button>
        {ms && <span className={`pill ${ok ? 'ok' : 'err'}`}>{ok ? 'ok' : 'error'} · {ms} ms</span>}
      </div>
      <textarea rows={8} value={sql} spellCheck={false} onChange={(e) => setSql(e.target.value)} onKeyDown={(e) => { if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') run() }} aria-label="SQL" />
      <div className="log" style={{ maxHeight: 420, whiteSpace: 'pre', color: ok ? 'var(--text)' : 'var(--err)' }}>{out}</div>
    </div>
  )
}
