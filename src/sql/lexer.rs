//! SQL tokenizer.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Bare identifier or keyword (keywords are matched case-insensitively by the parser).
    Word(String),
    /// "quoted identifier"
    Quoted(String),
    Str(String),
    Int(i64),
    Float(f64),
    Sym(&'static str),
    Eof,
}

const SYMS: &[&str] = &["<>", "!=", "<=", ">=", "||", "(", ")", ",", ";", ".", "*", "+", "-", "/", "%", "=", "<", ">"];

pub fn lex(sql: &str) -> Result<Vec<Tok>, String> {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'\'' {
            let mut s = Vec::new();
            i += 1;
            loop {
                match b.get(i) {
                    None => return Err("unterminated string literal".into()),
                    Some(b'\'') if b.get(i + 1) == Some(&b'\'') => {
                        s.push(b'\'');
                        i += 2;
                    }
                    Some(b'\'') => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(String::from_utf8_lossy(&s).into_owned()));
        } else if c == b'"' || c == b'`' {
            let end = b[i + 1..].iter().position(|&x| x == c).ok_or("unterminated quoted identifier")? + i + 1;
            out.push(Tok::Quoted(String::from_utf8_lossy(&b[i + 1..end]).into_owned()));
            i = end + 1;
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.' || b[i] == b'e' || b[i] == b'E' || ((b[i] == b'-' || b[i] == b'+') && matches!(b[i - 1], b'e' | b'E'))) {
                i += 1;
            }
            let text = &sql[start..i];
            if text.contains(['.', 'e', 'E']) {
                out.push(Tok::Float(text.parse().map_err(|_| format!("bad number '{text}'"))?));
            } else {
                out.push(Tok::Int(text.parse().map_err(|_| format!("integer out of range '{text}'"))?));
            }
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Word(sql[start..i].to_string()));
        } else if let Some(s) = SYMS.iter().find(|s| sql[i..].starts_with(**s)) {
            out.push(Tok::Sym(s));
            i += s.len();
        } else {
            return Err(format!("unexpected character '{}'", sql[i..].chars().next().unwrap()));
        }
    }
    out.push(Tok::Eof);
    Ok(out)
}
