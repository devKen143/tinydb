use std::io::{self, BufRead, Write};
use tinydb::{run_script, Database};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().cloned();
    let mut db = match &path {
        Some(p) if p != ":memory:" => match Database::open(p) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        _ => Database::open_memory(),
    };
    // non-interactive: `tinydb file.db "SQL"` or piped stdin
    if let Some(sql) = args.get(1) {
        let (ok, out) = run_script(&mut db, sql);
        println!("{out}");
        std::process::exit(if ok { 0 } else { 1 });
    }
    println!("tinydb shell — {}. End statements with ';'. Dot-commands: .tables .schema .stats .quit", path.as_deref().unwrap_or(":memory:"));
    let stdin = io::stdin();
    let mut buf = String::new();
    loop {
        let prompt = if !buf.is_empty() {
            "   ...> "
        } else if db.in_transaction() {
            "tinydb*> "
        } else {
            "tinydb> "
        };
        print!("{prompt}");
        io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let t = line.trim();
        if buf.is_empty() && (t == ".quit" || t == ".exit") {
            break;
        }
        if buf.is_empty() && t.starts_with('.') {
            println!("{}", run_script(&mut db, t).1);
            continue;
        }
        buf.push_str(&line);
        if !t.ends_with(';') {
            continue;
        }
        let (_, out) = run_script(&mut db, &buf);
        println!("{out}");
        buf.clear();
    }
}
