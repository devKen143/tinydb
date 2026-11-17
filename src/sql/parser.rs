//! Recursive-descent SQL parser.

use super::lexer::{lex, Tok};
use super::*;

pub struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

const RESERVED: &[&str] = &[
    "SELECT", "FROM", "WHERE", "GROUP", "BY", "HAVING", "ORDER", "LIMIT", "OFFSET", "JOIN", "INNER", "LEFT", "ON", "AND", "OR", "NOT", "AS", "ASC",
    "DESC", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "TABLE", "DROP", "INDEX", "IS", "NULL", "IN", "BETWEEN", "LIKE",
    "DISTINCT", "UNIQUE", "PRIMARY", "KEY", "OUTER", "CROSS",
];

/// Parses a script containing one or more `;`-separated statements.
pub fn parse(sql: &str) -> Result<Vec<Stmt>, String> {
    let mut p = Parser { toks: lex(sql)?, pos: 0 };
    let mut out = Vec::new();
    loop {
        while p.eat_sym(";") {}
        if p.peek() == &Tok::Eof {
            break;
        }
        out.push(p.statement()?);
        if !p.eat_sym(";") && p.peek() != &Tok::Eof {
            return Err(format!("expected ';' or end of input, found {}", p.describe()));
        }
    }
    Ok(out)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }
    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn describe(&self) -> String {
        match self.peek() {
            Tok::Word(w) => format!("'{w}'"),
            Tok::Quoted(w) => format!("\"{w}\""),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Int(i) => i.to_string(),
            Tok::Float(f) => f.to_string(),
            Tok::Sym(s) => format!("'{s}'"),
            Tok::Eof => "end of input".into(),
        }
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.next();
            true
        } else {
            false
        }
    }
    fn kw(&mut self, kw: &str) -> Result<(), String> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(format!("expected {kw}, found {}", self.describe()))
        }
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Tok::Sym(x) if *x == s) {
            self.next();
            true
        } else {
            false
        }
    }
    fn sym(&mut self, s: &str) -> Result<(), String> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(format!("expected '{s}', found {}", self.describe()))
        }
    }
    fn ident(&mut self) -> Result<String, String> {
        match self.peek().clone() {
            Tok::Word(w) if !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&w)) => {
                self.next();
                Ok(w.to_ascii_lowercase())
            }
            Tok::Quoted(w) => {
                self.next();
                Ok(w)
            }
            _ => Err(format!("expected a name, found {}", self.describe())),
        }
    }

    fn statement(&mut self) -> Result<Stmt, String> {
        if self.eat_kw("EXPLAIN") {
            return Ok(Stmt::Explain(Box::new(self.statement()?)));
        }
        if self.eat_kw("SELECT") {
            return Ok(Stmt::Select(Box::new(self.select()?)));
        }
        if self.eat_kw("INSERT") {
            self.kw("INTO")?;
            let table = self.ident()?;
            let columns = if self.eat_sym("(") {
                let c = self.list(|p| p.ident())?;
                self.sym(")")?;
                Some(c)
            } else {
                None
            };
            self.kw("VALUES")?;
            let mut rows = Vec::new();
            loop {
                self.sym("(")?;
                rows.push(self.list(|p| p.expr())?);
                self.sym(")")?;
                if !self.eat_sym(",") {
                    break;
                }
            }
            return Ok(Stmt::Insert { table, columns, rows });
        }
        if self.eat_kw("UPDATE") {
            let table = self.ident()?;
            self.kw("SET")?;
            let sets = self.list(|p| {
                let c = p.ident()?;
                p.sym("=")?;
                Ok((c, p.expr()?))
            })?;
            let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
            return Ok(Stmt::Update { table, sets, filter });
        }
        if self.eat_kw("DELETE") {
            self.kw("FROM")?;
            let table = self.ident()?;
            let filter = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
            return Ok(Stmt::Delete { table, filter });
        }
        if self.eat_kw("CREATE") {
            let unique = self.eat_kw("UNIQUE");
            if self.eat_kw("INDEX") {
                let name = self.ident()?;
                self.kw("ON")?;
                let table = self.ident()?;
                self.sym("(")?;
                let column = self.ident()?;
                self.sym(")")?;
                return Ok(Stmt::CreateIndex { name, table, column, unique });
            }
            if unique {
                return Err("expected INDEX after UNIQUE".into());
            }
            self.kw("TABLE")?;
            let if_not_exists = if self.eat_kw("IF") {
                self.kw("NOT")?;
                self.kw("EXISTS")?;
                true
            } else {
                false
            };
            let name = self.ident()?;
            self.sym("(")?;
            let columns = self.list(|p| p.column_def())?;
            self.sym(")")?;
            return Ok(Stmt::CreateTable { name, columns, if_not_exists });
        }
        if self.eat_kw("DROP") {
            if self.eat_kw("INDEX") {
                return Ok(Stmt::DropIndex { name: self.ident()? });
            }
            self.kw("TABLE")?;
            let if_exists = if self.eat_kw("IF") {
                self.kw("EXISTS")?;
                true
            } else {
                false
            };
            return Ok(Stmt::DropTable { name: self.ident()?, if_exists });
        }
        if self.eat_kw("BEGIN") {
            self.eat_kw("TRANSACTION");
            return Ok(Stmt::Begin);
        }
        if self.eat_kw("COMMIT") || self.eat_kw("END") {
            return Ok(Stmt::Commit);
        }
        if self.eat_kw("ROLLBACK") {
            return Ok(Stmt::Rollback);
        }
        Err(format!("unknown statement starting with {}", self.describe()))
    }

    fn column_def(&mut self) -> Result<ColumnDef, String> {
        let name = self.ident()?;
        let ty = match self.next() {
            Tok::Word(w) => Type::parse(&w).ok_or_else(|| format!("unknown type '{w}'"))?,
            _ => return Err(format!("expected a type for column '{name}'")),
        };
        if self.eat_sym("(") {
            // VARCHAR(255): accept and ignore the length
            while !self.eat_sym(")") {
                if self.next() == Tok::Eof {
                    return Err("unterminated type length".into());
                }
            }
        }
        let mut c = ColumnDef { name, ty, primary_key: false, not_null: false, unique: false, default: None };
        loop {
            if self.eat_kw("PRIMARY") {
                self.kw("KEY")?;
                c.primary_key = true;
                c.not_null = true;
            } else if self.eat_kw("NOT") {
                self.kw("NULL")?;
                c.not_null = true;
            } else if self.eat_kw("UNIQUE") {
                c.unique = true;
            } else if self.eat_kw("DEFAULT") {
                c.default = Some(match self.unary()? {
                    Expr::Lit(v) => v,
                    _ => return Err("DEFAULT must be a literal".into()),
                });
            } else if self.eat_kw("NULL") {
            } else {
                break;
            }
        }
        Ok(c)
    }

    fn list<T>(&mut self, mut f: impl FnMut(&mut Self) -> Result<T, String>) -> Result<Vec<T>, String> {
        let mut out = vec![f(self)?];
        while self.eat_sym(",") {
            out.push(f(self)?);
        }
        Ok(out)
    }

    fn table_ref(&mut self) -> Result<TableRef, String> {
        let name = self.ident()?;
        let alias = if self.eat_kw("AS") {
            Some(self.ident()?)
        } else if matches!(self.peek(), Tok::Word(w) if !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w))) {
            Some(self.ident()?)
        } else {
            None
        };
        Ok(TableRef { name, alias })
    }

    fn select(&mut self) -> Result<Select, String> {
        let mut s = Select { distinct: self.eat_kw("DISTINCT"), ..Default::default() };
        s.items = self.list(|p| {
            if p.eat_sym("*") {
                return Ok(SelectItem::Star);
            }
            // table.*
            if let (Tok::Word(t), Tok::Sym("."), Tok::Sym("*")) = (p.peek().clone(), p.toks.get(p.pos + 1).cloned().unwrap_or(Tok::Eof), p.toks.get(p.pos + 2).cloned().unwrap_or(Tok::Eof)) {
                p.pos += 3;
                return Ok(SelectItem::TableStar(t.to_ascii_lowercase()));
            }
            let e = p.expr()?;
            let alias = if p.eat_kw("AS") {
                Some(p.ident()?)
            } else if matches!(p.peek(), Tok::Word(w) if !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w))) {
                Some(p.ident()?)
            } else {
                None
            };
            Ok(SelectItem::Expr(e, alias))
        })?;
        if self.eat_kw("FROM") {
            s.from = Some(self.table_ref()?);
            loop {
                let left = if self.eat_kw("LEFT") {
                    self.eat_kw("OUTER");
                    true
                } else {
                    self.eat_kw("INNER");
                    false
                };
                if !self.eat_kw("JOIN") {
                    if left {
                        return Err("expected JOIN after LEFT".into());
                    }
                    break;
                }
                let table = self.table_ref()?;
                self.kw("ON")?;
                s.joins.push(Join { table, on: self.expr()?, left });
            }
        }
        if self.eat_kw("WHERE") {
            s.filter = Some(self.expr()?);
        }
        if self.eat_kw("GROUP") {
            self.kw("BY")?;
            s.group_by = self.list(|p| p.expr())?;
        }
        if self.eat_kw("HAVING") {
            s.having = Some(self.expr()?);
        }
        if self.eat_kw("ORDER") {
            self.kw("BY")?;
            s.order_by = self.list(|p| {
                let e = p.expr()?;
                let desc = if p.eat_kw("DESC") {
                    true
                } else {
                    p.eat_kw("ASC");
                    false
                };
                Ok((e, desc))
            })?;
        }
        if self.eat_kw("LIMIT") {
            s.limit = Some(self.expr()?);
            if self.eat_kw("OFFSET") {
                s.offset = Some(self.expr()?);
            }
        }
        Ok(s)
    }

    pub fn expr(&mut self) -> Result<Expr, String> {
        self.or()
    }
    fn or(&mut self) -> Result<Expr, String> {
        let mut l = self.and()?;
        while self.eat_kw("OR") {
            l = Expr::Binary(Box::new(l), BinOp::Or, Box::new(self.and()?));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut l = self.not()?;
        while self.eat_kw("AND") {
            l = Expr::Binary(Box::new(l), BinOp::And, Box::new(self.not()?));
        }
        Ok(l)
    }
    fn not(&mut self) -> Result<Expr, String> {
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.comparison()
    }
    fn comparison(&mut self) -> Result<Expr, String> {
        let l = self.additive()?;
        if self.eat_kw("IS") {
            let neg = self.eat_kw("NOT");
            self.kw("NULL")?;
            return Ok(Expr::IsNull(Box::new(l), neg));
        }
        let neg = self.eat_kw("NOT");
        if self.eat_kw("IN") {
            self.sym("(")?;
            let items = self.list(|p| p.expr())?;
            self.sym(")")?;
            return Ok(Expr::InList(Box::new(l), items, neg));
        }
        if self.eat_kw("BETWEEN") {
            let lo = self.additive()?;
            self.kw("AND")?;
            let hi = self.additive()?;
            return Ok(Expr::Between(Box::new(l), Box::new(lo), Box::new(hi), neg));
        }
        if self.eat_kw("LIKE") {
            return Ok(Expr::Like(Box::new(l), Box::new(self.additive()?), neg));
        }
        if neg {
            return Err(format!("expected IN, BETWEEN or LIKE after NOT, found {}", self.describe()));
        }
        let op = match self.peek() {
            Tok::Sym("=") => BinOp::Eq,
            Tok::Sym("<>") | Tok::Sym("!=") => BinOp::Ne,
            Tok::Sym("<") => BinOp::Lt,
            Tok::Sym("<=") => BinOp::Le,
            Tok::Sym(">") => BinOp::Gt,
            Tok::Sym(">=") => BinOp::Ge,
            _ => return Ok(l),
        };
        self.next();
        Ok(Expr::Binary(Box::new(l), op, Box::new(self.additive()?)))
    }
    fn additive(&mut self) -> Result<Expr, String> {
        let mut l = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Sym("+") => BinOp::Add,
                Tok::Sym("-") => BinOp::Sub,
                Tok::Sym("||") => BinOp::Concat,
                _ => return Ok(l),
            };
            self.next();
            l = Expr::Binary(Box::new(l), op, Box::new(self.multiplicative()?));
        }
    }
    fn multiplicative(&mut self) -> Result<Expr, String> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Sym("*") => BinOp::Mul,
                Tok::Sym("/") => BinOp::Div,
                Tok::Sym("%") => BinOp::Mod,
                _ => return Ok(l),
            };
            self.next();
            l = Expr::Binary(Box::new(l), op, Box::new(self.unary()?));
        }
    }
    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat_sym("-") {
            return Ok(match self.unary()? {
                Expr::Lit(Value::Int(i)) => Expr::Lit(Value::Int(-i)),
                Expr::Lit(Value::Real(r)) => Expr::Lit(Value::Real(-r)),
                e => Expr::Neg(Box::new(e)),
            });
        }
        if self.eat_sym("+") {
            return self.unary();
        }
        self.primary()
    }
    fn primary(&mut self) -> Result<Expr, String> {
        let t = self.peek().clone();
        Ok(match t {
            Tok::Int(i) => {
                self.next();
                Expr::Lit(Value::Int(i))
            }
            Tok::Float(f) => {
                self.next();
                Expr::Lit(Value::Real(f))
            }
            Tok::Str(s) => {
                self.next();
                Expr::Lit(Value::Text(s))
            }
            Tok::Sym("(") => {
                self.next();
                let e = self.expr()?;
                self.sym(")")?;
                e
            }
            Tok::Word(w) if w.eq_ignore_ascii_case("NULL") => {
                self.next();
                Expr::Lit(Value::Null)
            }
            Tok::Word(w) if w.eq_ignore_ascii_case("TRUE") || w.eq_ignore_ascii_case("FALSE") => {
                self.next();
                Expr::Lit(Value::Bool(w.eq_ignore_ascii_case("TRUE")))
            }
            Tok::Word(w) if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&w)) => {
                return Err(format!("expected an expression, found {}", self.describe()));
            }
            Tok::Word(_) | Tok::Quoted(_) => {
                // function call?
                if let (Tok::Word(name), Some(Tok::Sym("("))) = (&t, self.toks.get(self.pos + 1)) {
                    let name = name.to_ascii_uppercase();
                    self.pos += 2;
                    if name == "COUNT" && self.eat_sym("*") {
                        self.sym(")")?;
                        return Ok(Expr::CountStar);
                    }
                    let distinct = self.eat_kw("DISTINCT");
                    let args = if self.eat_sym(")") {
                        vec![]
                    } else {
                        let a = self.list(|p| p.expr())?;
                        self.sym(")")?;
                        a
                    };
                    return Ok(Expr::Func(name, args, distinct));
                }
                let first = self.ident()?;
                if self.eat_sym(".") {
                    Expr::Col(Some(first), self.ident()?)
                } else {
                    Expr::Col(None, first)
                }
            }
            _ => return Err(format!("expected an expression, found {}", self.describe())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_rich_select() {
        let s = parse(
            "SELECT u.name, COUNT(*) AS n, SUM(o.total) FROM users u LEFT JOIN orders o ON o.user_id = u.id \
             WHERE u.age BETWEEN 18 AND 65 AND u.name LIKE 'A%' GROUP BY u.name HAVING COUNT(*) > 1 ORDER BY n DESC LIMIT 10 OFFSET 5;",
        )
        .unwrap();
        let Stmt::Select(sel) = &s[0] else { panic!() };
        assert_eq!(sel.items.len(), 3);
        assert_eq!(sel.joins.len(), 1);
        assert!(sel.joins[0].left);
        assert_eq!(sel.from.as_ref().unwrap().binding(), "u");
        assert!(sel.having.is_some() && sel.limit.is_some() && sel.offset.is_some());
        assert!(sel.order_by[0].1);
    }

    #[test]
    fn parses_ddl_and_dml() {
        let s = parse(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, email VARCHAR(255) UNIQUE NOT NULL, score REAL DEFAULT 0.5);
             INSERT INTO t (email) VALUES ('a@x'), ('b@x');
             UPDATE t SET score = score * 2 WHERE id IN (1, 2);
             DELETE FROM t WHERE email IS NOT NULL;
             CREATE UNIQUE INDEX t_email ON t (email); BEGIN; COMMIT",
        )
        .unwrap();
        assert_eq!(s.len(), 7);
        let Stmt::CreateTable { columns, .. } = &s[0] else { panic!() };
        assert!(columns[0].primary_key && columns[1].unique && columns[1].not_null);
        assert_eq!(columns[2].default, Some(Value::Real(0.5)));
    }

    #[test]
    fn helpful_errors() {
        assert!(parse("SELEC 1").unwrap_err().contains("unknown statement"));
        assert!(parse("SELECT FROM t").unwrap_err().contains("expected an expression"));
        assert!(parse("SELECT 'oops").unwrap_err().contains("unterminated"));
        assert!(parse("INSERT INTO t VALUES (1").unwrap_err().contains("expected ')'"));
    }
}
