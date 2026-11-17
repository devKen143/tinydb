use tinydb::{Database, Value};

fn db() -> Database {
    let mut db = Database::open_memory();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE, age INTEGER, active BOOLEAN DEFAULT true);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total REAL, status TEXT);
         INSERT INTO users (name, email, age) VALUES ('Ada', 'ada@x.io', 36), ('Grace', 'grace@x.io', 45), ('Linus', 'linus@x.io', 28), ('Barbara', NULL, 52);
         INSERT INTO orders (user_id, total, status) VALUES (1, 9.5, 'paid'), (1, 20, 'paid'), (2, 5.25, 'refunded'), (3, 100, 'paid'), (1, 1, 'pending');",
    )
    .unwrap();
    db
}

fn strings(rows: Vec<Vec<Value>>) -> Vec<String> {
    rows.into_iter().map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|")).collect()
}

#[test]
fn select_where_order_limit() {
    let mut db = db();
    assert_eq!(strings(db.query("SELECT name, age FROM users WHERE age > 30 ORDER BY age DESC LIMIT 2").unwrap()), vec!["Barbara|52", "Grace|45"]);
    assert_eq!(strings(db.query("SELECT name FROM users WHERE name LIKE '%a' OR email IS NULL ORDER BY 1").unwrap()), vec!["Ada", "Barbara"]);
    assert_eq!(strings(db.query("SELECT id, name FROM users WHERE id IN (2, 4) ORDER BY id").unwrap()), vec!["2|Grace", "4|Barbara"]);
    assert_eq!(strings(db.query("SELECT UPPER(name) || '!' AS shout FROM users WHERE age BETWEEN 30 AND 40").unwrap()), vec!["ADA!"]);
    assert_eq!(strings(db.query("SELECT 1 + 2 * 3, 7 / 2, 7.0 / 2, 10 % 3, NULL + 1").unwrap()), vec!["7|3|3.5|1|NULL"]);
    assert_eq!(strings(db.query("SELECT active FROM users WHERE id = 1").unwrap()), vec!["true"], "DEFAULT applied");
}

#[test]
fn joins_and_aggregates() {
    let mut db = db();
    let rows = db
        .query(
            "SELECT u.name, COUNT(o.id) AS n, SUM(o.total) AS spent FROM users u LEFT JOIN orders o ON o.user_id = u.id
             GROUP BY u.name ORDER BY spent DESC",
        )
        .unwrap();
    assert_eq!(strings(rows), vec!["Linus|1|100.0", "Ada|3|30.5", "Grace|1|5.25", "Barbara|0|NULL"]);
    let rows = db.query("SELECT status, COUNT(*), AVG(total), MIN(total), MAX(total) FROM orders GROUP BY status HAVING COUNT(*) > 1").unwrap();
    assert_eq!(strings(rows), vec!["paid|3|43.166666666666664|9.5|100.0"]);
    assert_eq!(strings(db.query("SELECT COUNT(*), COUNT(email), COUNT(DISTINCT user_id) FROM users, orders").unwrap_or_default()).len(), 0);
    assert_eq!(strings(db.query("SELECT COUNT(DISTINCT user_id) FROM orders").unwrap()), vec!["3"]);
    assert_eq!(strings(db.query("SELECT u.name, o.total FROM orders o JOIN users u ON u.id = o.user_id WHERE o.total > 50").unwrap()), vec!["Linus|100.0"]);
    assert_eq!(strings(db.query("SELECT DISTINCT status FROM orders ORDER BY status").unwrap()), vec!["paid", "pending", "refunded"]);
}

#[test]
fn update_delete_and_constraints() {
    let mut db = db();
    db.execute("UPDATE users SET age = age + 1, name = 'Ada L.' WHERE id = 1").unwrap();
    assert_eq!(strings(db.query("SELECT name, age FROM users WHERE id = 1").unwrap()), vec!["Ada L.|37"]);
    assert!(db.execute("INSERT INTO users (name, email) VALUES ('Dup', 'ada@x.io')").unwrap_err().contains("UNIQUE constraint failed: users.email"));
    assert!(db.execute("INSERT INTO users (email) VALUES ('nobody@x.io')").unwrap_err().contains("NOT NULL"));
    assert!(db.execute("INSERT INTO users (id, name) VALUES (2, 'Clash')").unwrap_err().contains("UNIQUE constraint failed: users.id"));
    assert!(db.execute("UPDATE users SET email = 'grace@x.io' WHERE id = 3").unwrap_err().contains("UNIQUE"));
    assert!(db.execute("INSERT INTO users (name, age) VALUES ('Bad', 'old')").unwrap_err().contains("INTEGER"));
    assert_eq!(db.execute("DELETE FROM orders WHERE status <> 'paid'").unwrap()[0].message.as_deref(), Some("2 row(s) deleted"));
    assert_eq!(strings(db.query("SELECT COUNT(*) FROM orders").unwrap()), vec!["3"]);
    // a failed multi-row insert is atomic
    assert!(db.execute("INSERT INTO users (name, email) VALUES ('Ok', 'ok@x.io'), ('Bad', 'grace@x.io')").is_err());
    assert_eq!(strings(db.query("SELECT COUNT(*) FROM users WHERE name = 'Ok'").unwrap()), vec!["0"]);
}

#[test]
fn transactions() {
    let mut db = db();
    db.execute("BEGIN; INSERT INTO orders (user_id, total) VALUES (4, 7); DELETE FROM users WHERE id = 3;").unwrap();
    assert_eq!(strings(db.query("SELECT COUNT(*) FROM users").unwrap()), vec!["3"]);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(strings(db.query("SELECT COUNT(*) FROM users").unwrap()), vec!["4"]);
    assert_eq!(strings(db.query("SELECT COUNT(*) FROM orders").unwrap()), vec!["5"]);
    db.execute("BEGIN; UPDATE orders SET total = 0; COMMIT;").unwrap();
    assert_eq!(strings(db.query("SELECT SUM(total) FROM orders").unwrap()), vec!["0.0"]);
    assert!(db.execute("COMMIT").is_err());
}

#[test]
fn indexes_and_explain() {
    let mut db = db();
    let plan = |db: &mut Database, sql: &str| strings(db.query(&format!("EXPLAIN {sql}")).unwrap()).join(" / ");
    assert!(plan(&mut db, "SELECT * FROM users WHERE id = 2").contains("USING PRIMARY KEY"));
    assert!(plan(&mut db, "SELECT * FROM users WHERE email = 'ada@x.io'").contains("USING INDEX auto_users_email"));
    assert!(plan(&mut db, "SELECT * FROM users WHERE age = 36").contains("SCAN users"));
    db.execute("CREATE INDEX users_age ON users (age)").unwrap();
    assert!(plan(&mut db, "SELECT * FROM users WHERE age >= 40").contains("USING INDEX users_age (range)"));
    assert_eq!(strings(db.query("SELECT name FROM users WHERE age >= 40 ORDER BY name").unwrap()), vec!["Barbara", "Grace"]);
    assert!(plan(&mut db, "SELECT * FROM orders o JOIN users u ON u.id = o.user_id").contains("using PRIMARY KEY of users"));
    // index stays correct through updates and deletes
    db.execute("UPDATE users SET age = 99 WHERE name = 'Ada'; DELETE FROM users WHERE name = 'Grace'").unwrap();
    assert_eq!(strings(db.query("SELECT name FROM users WHERE age >= 40 ORDER BY age").unwrap()), vec!["Barbara", "Ada"]);
    assert!(db.execute("CREATE UNIQUE INDEX bad ON orders (user_id)").unwrap_err().contains("duplicate"));
}

#[test]
fn persistence_and_crash_recovery() {
    let path = std::env::temp_dir().join(format!("tinydb-test-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER)").unwrap();
        for i in 0..2000 {
            db.execute(&format!("INSERT INTO kv VALUES ('key{i:05}', {i})")).unwrap();
        }
        db.execute("CREATE INDEX kv_v ON kv (v)").unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(strings(db.query("SELECT COUNT(*), SUM(v) FROM kv").unwrap()), vec!["2000|1999000"]);
        assert_eq!(strings(db.query("SELECT k FROM kv WHERE v = 1234").unwrap()), vec!["key01234"]);
        // start a big change and crash half-way through writing it
        db.execute("BEGIN; UPDATE kv SET v = -1; DELETE FROM kv WHERE v = -1;").unwrap();
        db.pager_mut().commit_and_crash_midway().unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(strings(db.query("SELECT COUNT(*), SUM(v) FROM kv").unwrap()), vec!["2000|1999000"], "journal must restore the pre-transaction state");
    assert!(db.meta(".stats").unwrap().contains("kv: 2000 rows"));
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn error_messages() {
    let mut db = db();
    assert_eq!(db.execute("SELECT * FROM nope").unwrap_err(), "no such table: nope");
    assert_eq!(db.execute("SELECT nope FROM users").unwrap_err(), "no such column: nope");
    assert!(db.execute("SELECT id FROM users u JOIN orders o ON o.user_id = u.id").unwrap_err().contains("ambiguous"));
    assert!(db.execute("SELECT SUM(age) FROM users WHERE SUM(age) > 1").unwrap_err().contains("not allowed"));
    assert!(db.execute("CREATE TABLE users (x INT)").unwrap_err().contains("already exists"));
}
