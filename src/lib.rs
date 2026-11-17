//! tinydb — a small relational database engine.
//!
//! ```
//! let mut db = tinydb::Database::open_memory();
//! db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO t (name) VALUES ('ada');").unwrap();
//! assert_eq!(db.query("SELECT name FROM t WHERE id = 1").unwrap()[0][0].to_string(), "ada");
//! ```

pub mod btree;
pub mod db;
pub mod pager;
pub mod sql;
pub mod value;

pub use db::{format_result, Database, QueryResult};
pub use value::Value;

/// Runs a script and renders every result (used by the shell and the web demo).
pub fn run_script(db: &mut Database, script: &str) -> (bool, String) {
    let mut out = Vec::new();
    let mut ok = true;
    let mut sql = String::new();
    let flush = |db: &mut Database, sql: &mut String, out: &mut Vec<String>, ok: &mut bool| {
        if sql.trim().is_empty() {
            return;
        }
        match db.execute(sql) {
            Ok(results) => out.extend(results.iter().map(format_result)),
            Err(e) => {
                *ok = false;
                out.push(format!("Error: {e}"));
            }
        }
        sql.clear();
    };
    for line in script.lines() {
        if line.trim_start().starts_with('.') {
            flush(db, &mut sql, &mut out, &mut ok);
            match db.meta(line.trim()) {
                Ok(s) => out.push(s),
                Err(e) => {
                    ok = false;
                    out.push(format!("Error: {e}"));
                }
            }
        } else {
            sql.push_str(line);
            sql.push('\n');
        }
    }
    flush(db, &mut sql, &mut out, &mut ok);
    (ok, out.join("\n\n"))
}

#[cfg(target_arch = "wasm32")]
pub mod wasm {
    use std::cell::RefCell;
    thread_local! {
        static DB: RefCell<crate::Database> = RefCell::new(crate::Database::open_memory());
        static OUT: RefCell<Vec<u8>> = RefCell::new(Vec::new());
    }

    #[no_mangle]
    pub extern "C" fn alloc(len: usize) -> *mut u8 {
        let mut v = Vec::<u8>::with_capacity(len);
        let p = v.as_mut_ptr();
        std::mem::forget(v);
        p
    }

    #[no_mangle]
    pub unsafe extern "C" fn dealloc(ptr: *mut u8, len: usize) {
        drop(Vec::from_raw_parts(ptr, 0, len));
    }

    #[no_mangle]
    pub unsafe extern "C" fn run(ptr: *const u8, len: usize) -> u32 {
        let src = String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned();
        let (ok, text) = DB.with(|db| crate::run_script(&mut db.borrow_mut(), &src));
        OUT.with(|o| *o.borrow_mut() = text.into_bytes());
        (!ok) as u32
    }

    #[no_mangle]
    pub extern "C" fn reset() {
        DB.with(|db| *db.borrow_mut() = crate::Database::open_memory());
    }

    #[no_mangle]
    pub extern "C" fn output_ptr() -> *const u8 {
        OUT.with(|o| o.borrow().as_ptr())
    }

    #[no_mangle]
    pub extern "C" fn output_len() -> usize {
        OUT.with(|o| o.borrow().len())
    }
}
