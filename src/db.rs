//! The database: catalog, query planning and execution, constraints, indexes, transactions.

use crate::btree;
use crate::pager::{PageId, Pager, Storage, PAGE_SIZE};
use crate::sql::parser::parse;
use crate::sql::*;
use crate::value::{decode_row, encode_row, Reader, Type, Value, Writer};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;

const MAGIC: &[u8; 8] = b"TINYDB01";

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: Type,
    pub not_null: bool,
    pub unique: bool,
    pub pk: bool,
    pub default: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct IndexMeta {
    pub name: String,
    pub column: usize,
    pub unique: bool,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub id: i64,
    pub name: String,
    pub columns: Vec<Column>,
    pub root: PageId,
    pub next_rowid: i64,
    /// `INTEGER PRIMARY KEY` columns are aliases for the B+tree key.
    pub rowid_col: Option<usize>,
    pub indexes: Vec<IndexMeta>,
}

impl Table {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.str(&self.name);
        w.u32(self.root);
        w.i64(self.next_rowid);
        w.i64(self.rowid_col.map_or(-1, |c| c as i64));
        w.u32(self.columns.len() as u32);
        for c in &self.columns {
            w.str(&c.name);
            w.u8(match c.ty {
                Type::Integer => 0,
                Type::Real => 1,
                Type::Text => 2,
                Type::Boolean => 3,
            });
            w.u8(c.not_null as u8 | (c.unique as u8) << 1 | (c.pk as u8) << 2);
            w.value(c.default.as_ref().unwrap_or(&Value::Null));
        }
        w.u32(self.indexes.len() as u32);
        for i in &self.indexes {
            w.str(&i.name);
            w.u32(i.column as u32);
            w.u8(i.unique as u8);
        }
        w.0
    }

    fn decode(id: i64, b: &[u8]) -> Result<Table, String> {
        let mut r = Reader(b, 0);
        let name = r.str()?;
        let root = r.u32()?;
        let next_rowid = r.i64()?;
        let rc = r.i64()?;
        let n = r.u32()?;
        let mut columns = Vec::new();
        for _ in 0..n {
            let name = r.str()?;
            let ty = [Type::Integer, Type::Real, Type::Text, Type::Boolean][r.u8()? as usize % 4];
            let f = r.u8()?;
            let d = r.value()?;
            columns.push(Column { name, ty, not_null: f & 1 != 0, unique: f & 2 != 0, pk: f & 4 != 0, default: (!d.is_null()).then_some(d) });
        }
        let ni = r.u32()?;
        let mut indexes = Vec::new();
        for _ in 0..ni {
            indexes.push(IndexMeta { name: r.str()?, column: r.u32()? as usize, unique: r.u8()? != 0 });
        }
        Ok(Table { id, name, columns, root, next_rowid, rowid_col: (rc >= 0).then_some(rc as usize), indexes })
    }

    fn col(&self, name: &str) -> Result<usize, String> {
        self.columns.iter().position(|c| c.name == name).ok_or_else(|| format!("no such column: {}.{name}", self.name))
    }

    pub fn ddl(&self) -> String {
        let cols: Vec<String> = self
            .columns
            .iter()
            .map(|c| {
                let mut s = format!("{} {}", c.name, c.ty.name());
                if c.pk {
                    s += " PRIMARY KEY";
                } else {
                    if c.not_null {
                        s += " NOT NULL";
                    }
                    if c.unique {
                        s += " UNIQUE";
                    }
                }
                if let Some(d) = &c.default {
                    s += &format!(" DEFAULT {}", lit(d));
                }
                s
            })
            .collect();
        let mut out = format!("CREATE TABLE {} ({});", self.name, cols.join(", "));
        for i in self.indexes.iter().filter(|i| !i.name.starts_with("auto_")) {
            out += &format!("\nCREATE {}INDEX {} ON {} ({});", if i.unique { "UNIQUE " } else { "" }, i.name, self.name, self.columns[i.column].name);
        }
        out
    }
}

fn lit(v: &Value) -> String {
    match v {
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        o => o.to_string(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub message: Option<String>,
}

type Row = Vec<Value>;
type IndexData = BTreeMap<Value, BTreeSet<i64>>;

/// Name resolution scope: (binding, column) for each position in a row.
struct Scope {
    cols: Vec<(String, String)>,
}

impl Scope {
    fn resolve(&self, q: &Option<String>, name: &str) -> Result<usize, String> {
        let mut hit = None;
        for (i, (b, c)) in self.cols.iter().enumerate() {
            if c == name && q.as_ref().map_or(true, |q| q == b) {
                if hit.is_some() && q.is_none() {
                    return Err(format!("ambiguous column name: {name}"));
                }
                hit = Some(i);
            }
        }
        hit.ok_or_else(|| match q {
            Some(q) => format!("no such column: {q}.{name}"),
            None => format!("no such column: {name}"),
        })
    }
}

/// How rows of a table are located.
#[derive(Debug, Clone)]
enum Access {
    Scan,
    Rowids(Vec<i64>),
    RowidRange(i64, i64),
    IndexEq(String, Vec<Value>),
    IndexRange(String, Bound<Value>, Bound<Value>),
}

pub struct Database {
    pager: Pager,
    catalog_root: PageId,
    next_table_id: i64,
    tables: BTreeMap<String, Table>,
    index_data: HashMap<String, IndexData>,
    in_txn: bool,
}

impl Database {
    pub fn open_with(storage: Box<dyn Storage>) -> Result<Database, String> {
        let mut pager = Pager::open(storage).map_err(|e| e.to_string())?;
        let e = |e: std::io::Error| e.to_string();
        if pager.page_count() == 0 {
            pager.allocate().map_err(e)?;
            let root = btree::create(&mut pager).map_err(e)?;
            let mut header = vec![0u8; PAGE_SIZE];
            header[..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&root.to_le_bytes());
            header[12..20].copy_from_slice(&1i64.to_le_bytes());
            pager.write(0, header).map_err(e)?;
            pager.commit().map_err(e)?;
        }
        let header = pager.get(0).map_err(e)?.clone();
        if &header[..8] != MAGIC {
            return Err("not a tinydb database file".into());
        }
        let catalog_root = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let next_table_id = i64::from_le_bytes(header[12..20].try_into().unwrap());
        let mut db = Database { pager, catalog_root, next_table_id, tables: BTreeMap::new(), index_data: HashMap::new(), in_txn: false };
        db.load_catalog()?;
        Ok(db)
    }

    pub fn open_memory() -> Database {
        Self::open_with(Box::new(crate::pager::MemStorage::default())).expect("in-memory open cannot fail")
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Database, String> {
        Self::open_with(Box::new(crate::pager::FileStorage::open(path.as_ref()).map_err(|e| e.to_string())?))
    }

    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.values()
    }

    pub fn in_transaction(&self) -> bool {
        self.in_txn
    }

    pub fn page_stats(&self) -> (u32, u64, u64) {
        (self.pager.page_count(), self.pager.reads, self.pager.writes)
    }

    #[doc(hidden)]
    pub fn pager_mut(&mut self) -> &mut Pager {
        &mut self.pager
    }

    fn load_catalog(&mut self) -> Result<(), String> {
        self.tables.clear();
        self.index_data.clear();
        let header = self.pager.get(0).map_err(|e| e.to_string())?.clone();
        self.next_table_id = i64::from_le_bytes(header[12..20].try_into().unwrap());
        for (id, raw) in btree::scan(&mut self.pager, self.catalog_root).map_err(|e| e.to_string())? {
            let t = Table::decode(id, &raw)?;
            self.tables.insert(t.name.clone(), t);
        }
        let tables: Vec<Table> = self.tables.values().cloned().collect();
        for t in tables {
            if t.indexes.is_empty() {
                continue;
            }
            let rows = self.scan(&t)?;
            for idx in &t.indexes {
                let mut map = IndexData::new();
                for (rid, row) in &rows {
                    map.entry(row[idx.column].clone()).or_default().insert(*rid);
                }
                self.index_data.insert(idx.name.clone(), map);
            }
        }
        Ok(())
    }

    fn save_table(&mut self, t: &Table) -> Result<(), String> {
        btree::insert(&mut self.pager, self.catalog_root, t.id, &t.encode()).map_err(|e| e.to_string())?;
        self.tables.insert(t.name.clone(), t.clone());
        Ok(())
    }

    fn save_header(&mut self) -> Result<(), String> {
        let mut h = self.pager.get(0).map_err(|e| e.to_string())?.clone();
        h[12..20].copy_from_slice(&self.next_table_id.to_le_bytes());
        self.pager.write(0, h).map_err(|e| e.to_string())
    }

    fn table(&self, name: &str) -> Result<Table, String> {
        self.tables.get(name).cloned().ok_or_else(|| format!("no such table: {name}"))
    }

    fn scan(&mut self, t: &Table) -> Result<Vec<(i64, Row)>, String> {
        btree::scan(&mut self.pager, t.root).map_err(|e| e.to_string())?.into_iter().map(|(k, v)| Ok((k, decode_row(&v)?))).collect()
    }

    fn get_row(&mut self, t: &Table, rowid: i64) -> Result<Option<Row>, String> {
        btree::get(&mut self.pager, t.root, rowid).map_err(|e| e.to_string())?.map(|v| decode_row(&v)).transpose()
    }

    fn rollback_state(&mut self) {
        self.pager.rollback();
        self.in_txn = false;
        let _ = self.load_catalog();
    }

    /// Runs every statement in `sql`, returning one result per statement.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, String> {
        let stmts = parse(sql)?;
        let mut out = Vec::new();
        for s in stmts {
            out.push(self.run(s)?);
        }
        Ok(out)
    }

    /// Convenience: runs one query and returns its rows.
    pub fn query(&mut self, sql: &str) -> Result<Vec<Row>, String> {
        Ok(self.execute(sql)?.pop().map(|r| r.rows).unwrap_or_default())
    }

    fn run(&mut self, s: Stmt) -> Result<QueryResult, String> {
        let msg = |m: &str| Ok(QueryResult { message: Some(m.into()), ..Default::default() });
        match s {
            Stmt::Begin => {
                if self.in_txn {
                    return Err("cannot start a transaction within a transaction".into());
                }
                self.in_txn = true;
                msg("BEGIN")
            }
            Stmt::Commit => {
                if !self.in_txn {
                    return Err("cannot commit - no transaction is active".into());
                }
                self.pager.commit().map_err(|e| e.to_string())?;
                self.in_txn = false;
                msg("COMMIT")
            }
            Stmt::Rollback => {
                if !self.in_txn {
                    return Err("cannot rollback - no transaction is active".into());
                }
                self.rollback_state();
                msg("ROLLBACK")
            }
            other => match self.exec(other) {
                Ok(r) => {
                    if !self.in_txn {
                        if let Err(e) = self.pager.commit() {
                            self.rollback_state();
                            return Err(format!("commit failed: {e}"));
                        }
                    }
                    Ok(r)
                }
                Err(e) => {
                    // statements are atomic; an error inside BEGIN..COMMIT aborts the transaction
                    let was = self.in_txn;
                    self.rollback_state();
                    Err(if was { format!("{e} (transaction rolled back)") } else { e })
                }
            },
        }
    }

    fn exec(&mut self, s: Stmt) -> Result<QueryResult, String> {
        let done = |m: String| Ok(QueryResult { message: Some(m), ..Default::default() });
        match s {
            Stmt::CreateTable { name, columns, if_not_exists } => {
                if self.tables.contains_key(&name) {
                    return if if_not_exists { done("table already exists".into()) } else { Err(format!("table {name} already exists")) };
                }
                if columns.iter().filter(|c| c.primary_key).count() > 1 {
                    return Err("a table can have only one PRIMARY KEY column".into());
                }
                for (i, c) in columns.iter().enumerate() {
                    if columns[..i].iter().any(|o| o.name == c.name) {
                        return Err(format!("duplicate column name: {}", c.name));
                    }
                }
                let root = btree::create(&mut self.pager).map_err(|e| e.to_string())?;
                let rowid_col = columns.iter().position(|c| c.primary_key && c.ty == Type::Integer);
                let cols: Vec<Column> = columns
                    .iter()
                    .map(|c| Column { name: c.name.clone(), ty: c.ty, not_null: c.not_null, unique: c.unique, pk: c.primary_key, default: c.default.clone() })
                    .collect();
                let mut indexes = Vec::new();
                for (i, c) in cols.iter().enumerate() {
                    if Some(i) != rowid_col && (c.unique || c.pk) {
                        indexes.push(IndexMeta { name: format!("auto_{name}_{}", c.name), column: i, unique: true });
                    }
                }
                for idx in &indexes {
                    self.index_data.insert(idx.name.clone(), IndexData::new());
                }
                let t = Table { id: self.next_table_id, name: name.clone(), columns: cols, root, next_rowid: 1, rowid_col, indexes };
                self.next_table_id += 1;
                self.save_header()?;
                self.save_table(&t)?;
                done(format!("created table {name}"))
            }
            Stmt::DropTable { name, if_exists } => {
                let Some(t) = self.tables.get(&name).cloned() else {
                    return if if_exists { done("table does not exist".into()) } else { Err(format!("no such table: {name}")) };
                };
                btree::delete(&mut self.pager, self.catalog_root, t.id).map_err(|e| e.to_string())?;
                for i in &t.indexes {
                    self.index_data.remove(&i.name);
                }
                self.tables.remove(&name);
                done(format!("dropped table {name}"))
            }
            Stmt::CreateIndex { name, table, column, unique } => {
                if self.tables.values().any(|t| t.indexes.iter().any(|i| i.name == name)) {
                    return Err(format!("index {name} already exists"));
                }
                let mut t = self.table(&table)?;
                let col = t.col(&column)?;
                let mut map = IndexData::new();
                for (rid, row) in self.scan(&t)? {
                    let set = map.entry(row[col].clone()).or_default();
                    if unique && !row[col].is_null() && !set.is_empty() {
                        return Err(format!("UNIQUE constraint failed: cannot index {table}.{column}, duplicate value {}", row[col]));
                    }
                    set.insert(rid);
                }
                let n = map.values().map(BTreeSet::len).sum::<usize>();
                self.index_data.insert(name.clone(), map);
                t.indexes.push(IndexMeta { name: name.clone(), column: col, unique });
                self.save_table(&t)?;
                done(format!("created index {name} on {table}({column}) with {n} entries"))
            }
            Stmt::DropIndex { name } => {
                let mut t = self.tables.values().find(|t| t.indexes.iter().any(|i| i.name == name)).cloned().ok_or_else(|| format!("no such index: {name}"))?;
                if name.starts_with("auto_") {
                    return Err("cannot drop an index that enforces a UNIQUE/PRIMARY KEY constraint".into());
                }
                t.indexes.retain(|i| i.name != name);
                self.index_data.remove(&name);
                self.save_table(&t)?;
                done(format!("dropped index {name}"))
            }
            Stmt::Insert { table, columns, rows } => self.insert(&table, columns, rows),
            Stmt::Select(sel) => self.select(&sel),
            Stmt::Update { table, sets, filter } => self.update(&table, &sets, filter.as_ref()),
            Stmt::Delete { table, filter } => self.delete(&table, filter.as_ref()),
            Stmt::Explain(inner) => self.explain(&inner),
            Stmt::Begin | Stmt::Commit | Stmt::Rollback => unreachable!(),
        }
    }

    // ------------------------------------------------------------------ writes

    fn check_and_coerce(&self, t: &Table, mut row: Row) -> Result<Row, String> {
        for (i, c) in t.columns.iter().enumerate() {
            let v = std::mem::replace(&mut row[i], Value::Null);
            row[i] = v.coerce(c.ty).map_err(|e| format!("{e} ({}.{})", t.name, c.name))?;
            if c.not_null && row[i].is_null() && Some(i) != t.rowid_col {
                return Err(format!("NOT NULL constraint failed: {}.{}", t.name, c.name));
            }
        }
        Ok(row)
    }

    fn check_unique(&self, t: &Table, row: &Row, rowid: i64) -> Result<(), String> {
        for idx in t.indexes.iter().filter(|i| i.unique) {
            let v = &row[idx.column];
            if v.is_null() {
                continue;
            }
            if let Some(set) = self.index_data.get(&idx.name).and_then(|m| m.get(v)) {
                if set.iter().any(|&r| r != rowid) {
                    return Err(format!("UNIQUE constraint failed: {}.{}", t.name, t.columns[idx.column].name));
                }
            }
        }
        Ok(())
    }

    fn index_add(&mut self, t: &Table, row: &Row, rowid: i64) {
        for idx in &t.indexes {
            self.index_data.entry(idx.name.clone()).or_default().entry(row[idx.column].clone()).or_default().insert(rowid);
        }
    }

    fn index_remove(&mut self, t: &Table, row: &Row, rowid: i64) {
        for idx in &t.indexes {
            if let Some(m) = self.index_data.get_mut(&idx.name) {
                if let Some(set) = m.get_mut(&row[idx.column]) {
                    set.remove(&rowid);
                    if set.is_empty() {
                        m.remove(&row[idx.column]);
                    }
                }
            }
        }
    }

    fn write_row(&mut self, t: &Table, rowid: i64, row: &Row) -> Result<(), String> {
        btree::insert(&mut self.pager, t.root, rowid, &encode_row(row)).map_err(|e| e.to_string()).map(|_| ())
    }

    fn insert(&mut self, table: &str, columns: Option<Vec<String>>, rows: Vec<Vec<Expr>>) -> Result<QueryResult, String> {
        let mut t = self.table(table)?;
        let targets: Vec<usize> = match &columns {
            Some(cs) => cs.iter().map(|c| t.col(c)).collect::<Result<_, _>>()?,
            None => (0..t.columns.len()).collect(),
        };
        let empty = Scope { cols: vec![] };
        let mut count = 0;
        for exprs in rows {
            if exprs.len() != targets.len() {
                return Err(format!("{} values for {} columns", exprs.len(), targets.len()));
            }
            let mut row: Row = t.columns.iter().map(|c| c.default.clone().unwrap_or(Value::Null)).collect();
            for (e, &ci) in exprs.iter().zip(&targets) {
                row[ci] = eval(e, &empty, &[], None)?;
            }
            let mut row = self.check_and_coerce(&t, row)?;
            let rowid = match t.rowid_col.map(|c| row[c].clone()) {
                Some(Value::Int(id)) => id,
                _ => t.next_rowid,
            };
            if let Some(c) = t.rowid_col {
                row[c] = Value::Int(rowid);
            }
            if self.get_row(&t, rowid)?.is_some() {
                let col = t.rowid_col.map_or("rowid".to_string(), |c| t.columns[c].name.clone());
                return Err(format!("UNIQUE constraint failed: {}.{col}", t.name));
            }
            self.check_unique(&t, &row, rowid)?;
            self.write_row(&t, rowid, &row)?;
            self.index_add(&t, &row, rowid);
            t.next_rowid = t.next_rowid.max(rowid.saturating_add(1));
            count += 1;
        }
        self.save_table(&t)?;
        Ok(QueryResult { message: Some(format!("{count} row(s) inserted")), ..Default::default() })
    }

    fn matching_rows(&mut self, t: &Table, filter: Option<&Expr>) -> Result<Vec<(i64, Row)>, String> {
        let scope = table_scope(t, &t.name);
        let access = plan_access(t, &t.name, filter, &scope);
        let rows = self.fetch(t, &access)?;
        let mut out = Vec::new();
        for (rid, row) in rows {
            if let Some(f) = filter {
                if !eval(f, &scope, &row, None)?.truthy() {
                    continue;
                }
            }
            out.push((rid, row));
        }
        Ok(out)
    }

    fn update(&mut self, table: &str, sets: &[(String, Expr)], filter: Option<&Expr>) -> Result<QueryResult, String> {
        let t = self.table(table)?;
        let scope = table_scope(&t, &t.name);
        let targets: Vec<(usize, &Expr)> = sets.iter().map(|(c, e)| Ok((t.col(c)?, e))).collect::<Result<_, String>>()?;
        let rows = self.matching_rows(&t, filter)?;
        let n = rows.len();
        for (rid, old) in rows {
            let mut new = old.clone();
            for (ci, e) in &targets {
                new[*ci] = eval(e, &scope, &old, None)?;
            }
            let new = self.check_and_coerce(&t, new)?;
            let new_rid = match t.rowid_col.map(|c| &new[c]) {
                Some(Value::Int(id)) => *id,
                Some(_) => return Err(format!("NOT NULL constraint failed: {}.{}", t.name, t.columns[t.rowid_col.unwrap()].name)),
                None => rid,
            };
            if new_rid != rid && self.get_row(&t, new_rid)?.is_some() {
                return Err(format!("UNIQUE constraint failed: {}.{}", t.name, t.columns[t.rowid_col.unwrap()].name));
            }
            self.index_remove(&t, &old, rid);
            if let Err(e) = self.check_unique(&t, &new, new_rid) {
                return Err(e);
            }
            if new_rid != rid {
                btree::delete(&mut self.pager, t.root, rid).map_err(|e| e.to_string())?;
            }
            self.write_row(&t, new_rid, &new)?;
            self.index_add(&t, &new, new_rid);
        }
        if let Some(c) = t.rowid_col {
            // keep next_rowid above any id an UPDATE may have set
            let max = self.scan(&t)?.last().map(|(k, _)| *k).unwrap_or(0);
            let mut t2 = self.table(table)?;
            if max >= t2.next_rowid {
                t2.next_rowid = max + 1;
                self.save_table(&t2)?;
            }
            let _ = c;
        }
        Ok(QueryResult { message: Some(format!("{n} row(s) updated")), ..Default::default() })
    }

    fn delete(&mut self, table: &str, filter: Option<&Expr>) -> Result<QueryResult, String> {
        let t = self.table(table)?;
        let rows = self.matching_rows(&t, filter)?;
        for (rid, row) in &rows {
            btree::delete(&mut self.pager, t.root, *rid).map_err(|e| e.to_string())?;
            self.index_remove(&t, row, *rid);
        }
        Ok(QueryResult { message: Some(format!("{} row(s) deleted", rows.len())), ..Default::default() })
    }

    // ------------------------------------------------------------------ reads

    fn fetch(&mut self, t: &Table, a: &Access) -> Result<Vec<(i64, Row)>, String> {
        let e = |e: std::io::Error| e.to_string();
        Ok(match a {
            Access::Scan => self.scan(t)?,
            Access::Rowids(ids) => {
                let mut out = Vec::new();
                for &id in ids {
                    if let Some(r) = self.get_row(t, id)? {
                        out.push((id, r));
                    }
                }
                out
            }
            Access::RowidRange(lo, hi) => btree::range(&mut self.pager, t.root, *lo, *hi).map_err(e)?.into_iter().map(|(k, v)| Ok((k, decode_row(&v)?))).collect::<Result<_, String>>()?,
            Access::IndexEq(name, _) | Access::IndexRange(name, ..) => {
                let map = self.index_data.get(name).ok_or("index missing")?;
                let ids: Vec<i64> = match a {
                    Access::IndexEq(_, vals) => vals.iter().filter_map(|v| map.get(v)).flatten().copied().collect(),
                    Access::IndexRange(_, lo, hi) => map.range((lo.clone(), hi.clone())).flat_map(|(_, s)| s.iter().copied()).collect(),
                    _ => unreachable!(),
                };
                let mut out = Vec::new();
                for id in ids {
                    if let Some(r) = self.get_row(t, id)? {
                        out.push((id, r));
                    }
                }
                out
            }
        })
    }

    fn select(&mut self, s: &Select) -> Result<QueryResult, String> {
        // 1. FROM + JOINs
        let (mut scope, mut rows) = match &s.from {
            None => (Scope { cols: vec![] }, vec![vec![]]),
            Some(tr) => {
                let t = self.table(&tr.name)?;
                let scope = table_scope(&t, tr.binding());
                let access = if s.joins.is_empty() { plan_access(&t, tr.binding(), s.filter.as_ref(), &scope) } else { Access::Scan };
                let rows = self.fetch(&t, &access)?.into_iter().map(|(_, r)| r).collect();
                (scope, rows)
            }
        };
        for j in &s.joins {
            let rt = self.table(&j.table.name)?;
            let rscope = table_scope(&rt, j.table.binding());
            let mut combined = Scope { cols: scope.cols.clone() };
            combined.cols.extend(rscope.cols.iter().cloned());
            let lookup = join_lookup(&rt, j.table.binding(), &j.on, &scope);
            let all_right = if lookup.is_none() { Some(self.scan(&rt)?) } else { None };
            let mut out = Vec::new();
            for left in rows {
                let candidates: Vec<Row> = match (&lookup, &all_right) {
                    (Some((col, key_expr)), _) => {
                        let key = eval(key_expr, &scope, &left, None)?;
                        let ids: Vec<i64> = if Some(*col) == rt.rowid_col {
                            match key {
                                Value::Int(i) => vec![i],
                                _ => vec![],
                            }
                        } else {
                            let idx = rt.indexes.iter().find(|i| i.column == *col).unwrap();
                            self.index_data.get(&idx.name).and_then(|m| m.get(&key)).map(|s| s.iter().copied().collect()).unwrap_or_default()
                        };
                        let mut v = Vec::new();
                        for id in ids {
                            if let Some(r) = self.get_row(&rt, id)? {
                                v.push(r);
                            }
                        }
                        v
                    }
                    (None, Some(all)) => all.iter().map(|(_, r)| r.clone()).collect(),
                    _ => unreachable!(),
                };
                let mut matched = false;
                for right in candidates {
                    let mut row = left.clone();
                    row.extend(right);
                    if eval(&j.on, &combined, &row, None)?.truthy() {
                        matched = true;
                        out.push(row);
                    }
                }
                if !matched && j.left {
                    let mut row = left.clone();
                    row.extend(std::iter::repeat(Value::Null).take(rt.columns.len()));
                    out.push(row);
                }
            }
            rows = out;
            scope = combined;
        }

        // 2. WHERE
        if let Some(f) = &s.filter {
            let mut kept = Vec::new();
            for r in rows {
                if eval(f, &scope, &r, None)?.truthy() {
                    kept.push(r);
                }
            }
            rows = kept;
        }

        // 3. projection list
        let mut items: Vec<(Expr, String)> = Vec::new();
        for it in &s.items {
            match it {
                SelectItem::Star => {
                    if scope.cols.is_empty() {
                        return Err("SELECT * requires a FROM clause".into());
                    }
                    for (b, c) in &scope.cols {
                        items.push((Expr::Col(Some(b.clone()), c.clone()), c.clone()));
                    }
                }
                SelectItem::TableStar(t) => {
                    let before = items.len();
                    for (b, c) in scope.cols.iter().filter(|(b, _)| b == t) {
                        items.push((Expr::Col(Some(b.clone()), c.clone()), c.clone()));
                    }
                    if items.len() == before {
                        return Err(format!("no such table: {t}"));
                    }
                }
                SelectItem::Expr(e, alias) => items.push((e.clone(), alias.clone().unwrap_or_else(|| e.label()))),
            }
        }
        let columns: Vec<String> = items.iter().map(|i| i.1.clone()).collect();

        // ORDER BY may refer to output aliases or positions
        enum Key<'a> {
            Output(usize),
            Expr(&'a Expr),
        }
        let keys: Vec<(Key, bool)> = s
            .order_by
            .iter()
            .map(|(e, desc)| {
                let k = match e {
                    Expr::Lit(Value::Int(n)) if *n >= 1 && (*n as usize) <= items.len() => Key::Output(*n as usize - 1),
                    Expr::Col(None, name) if columns.iter().any(|c| c == name) && scope.resolve(&None, name).is_err() => Key::Output(columns.iter().position(|c| c == name).unwrap()),
                    e => Key::Expr(e),
                };
                (k, *desc)
            })
            .collect();

        // 4. aggregate or plain projection
        let aggregate = !s.group_by.is_empty() || s.having.is_some() || items.iter().any(|(e, _)| e.has_aggregate());
        let mut out: Vec<(Row, Row)> = Vec::new(); // (sort keys, output)
        let project = |row: &Row, group: Option<&[Row]>| -> Result<(Row, Row), String> {
            let o: Row = items.iter().map(|(e, _)| eval(e, &scope, row, group)).collect::<Result<_, _>>()?;
            let k: Row = keys
                .iter()
                .map(|(k, _)| match k {
                    Key::Output(i) => Ok(o[*i].clone()),
                    Key::Expr(e) => eval(e, &scope, row, group),
                })
                .collect::<Result<_, _>>()?;
            Ok((k, o))
        };
        if aggregate {
            let mut groups: BTreeMap<Row, Vec<Row>> = BTreeMap::new();
            if s.group_by.is_empty() {
                groups.insert(vec![], rows);
            } else {
                for r in rows {
                    let k: Row = s.group_by.iter().map(|e| eval(e, &scope, &r, None)).collect::<Result<_, _>>()?;
                    groups.entry(k).or_default().push(r);
                }
            }
            let null_row: Row = vec![Value::Null; scope.cols.len()];
            for (_, g) in groups {
                let first = g.first().unwrap_or(&null_row).clone();
                if let Some(h) = &s.having {
                    if !eval(h, &scope, &first, Some(&g))?.truthy() {
                        continue;
                    }
                }
                out.push(project(&first, Some(&g))?);
            }
        } else {
            for r in &rows {
                out.push(project(r, None)?);
            }
        }

        // 5. ORDER BY, DISTINCT, OFFSET/LIMIT
        if !keys.is_empty() {
            out.sort_by(|a, b| {
                for (i, (_, desc)) in keys.iter().enumerate() {
                    let o = a.0[i].cmp(&b.0[i]);
                    if o != std::cmp::Ordering::Equal {
                        return if *desc { o.reverse() } else { o };
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        let mut result: Vec<Row> = out.into_iter().map(|(_, o)| o).collect();
        if s.distinct {
            let mut seen = BTreeSet::new();
            result.retain(|r| seen.insert(r.clone()));
        }
        let empty = Scope { cols: vec![] };
        let num = |e: &Option<Expr>| -> Result<Option<usize>, String> {
            match e {
                None => Ok(None),
                Some(e) => match eval(e, &empty, &[], None)? {
                    Value::Int(n) if n >= 0 => Ok(Some(n as usize)),
                    v => Err(format!("LIMIT/OFFSET must be a non-negative integer, got {v}")),
                },
            }
        };
        let offset = num(&s.offset)?.unwrap_or(0);
        let limit = num(&s.limit)?.unwrap_or(usize::MAX);
        let result: Vec<Row> = result.into_iter().skip(offset).take(limit).collect();
        Ok(QueryResult { columns, rows: result, message: None })
    }

    fn explain(&mut self, s: &Stmt) -> Result<QueryResult, String> {
        let mut lines = Vec::new();
        let describe = |t: &Table, a: &Access| match a {
            Access::Scan => format!("SCAN {} (full table scan)", t.name),
            Access::Rowids(ids) => format!("SEARCH {} USING PRIMARY KEY ({} key lookup(s))", t.name, ids.len()),
            Access::RowidRange(..) => format!("SEARCH {} USING PRIMARY KEY RANGE", t.name),
            Access::IndexEq(n, v) => format!("SEARCH {} USING INDEX {n} ({} value(s))", t.name, v.len()),
            Access::IndexRange(n, ..) => format!("SEARCH {} USING INDEX {n} (range)", t.name),
        };
        match s {
            Stmt::Select(sel) => {
                if let Some(tr) = &sel.from {
                    let t = self.table(&tr.name)?;
                    let scope = table_scope(&t, tr.binding());
                    let a = if sel.joins.is_empty() { plan_access(&t, tr.binding(), sel.filter.as_ref(), &scope) } else { Access::Scan };
                    lines.push(describe(&t, &a));
                    let mut left_scope = scope;
                    for j in &sel.joins {
                        let rt = self.table(&j.table.name)?;
                        let how = match join_lookup(&rt, j.table.binding(), &j.on, &left_scope) {
                            Some((c, _)) if Some(c) == rt.rowid_col => format!("using PRIMARY KEY of {}", rt.name),
                            Some((c, _)) => format!("using INDEX on {}.{}", rt.name, rt.columns[c].name),
                            None => format!("scanning {}", rt.name),
                        };
                        lines.push(format!("{} JOIN {} ({how})", if j.left { "LEFT NESTED LOOP" } else { "NESTED LOOP" }, rt.name));
                        left_scope.cols.extend(table_scope(&rt, j.table.binding()).cols);
                    }
                }
                if sel.filter.is_some() {
                    lines.push("FILTER rows by WHERE".into());
                }
                let agg = !sel.group_by.is_empty() || sel.items.iter().any(|i| matches!(i, SelectItem::Expr(e, _) if e.has_aggregate()));
                if agg {
                    lines.push(if sel.group_by.is_empty() { "AGGREGATE (single group)".into() } else { format!("GROUP BY {} key(s)", sel.group_by.len()) });
                }
                if !sel.order_by.is_empty() {
                    lines.push("SORT results".into());
                }
                if sel.limit.is_some() {
                    lines.push("LIMIT".into());
                }
            }
            Stmt::Update { table, filter, .. } | Stmt::Delete { table, filter } => {
                let t = self.table(table)?;
                let scope = table_scope(&t, &t.name);
                lines.push(describe(&t, &plan_access(&t, &t.name, filter.as_ref(), &scope)));
                lines.push(format!("{} matching rows", if matches!(s, Stmt::Update { .. }) { "UPDATE" } else { "DELETE" }));
            }
            Stmt::Insert { table, rows, .. } => lines.push(format!("INSERT {} row(s) INTO {table}", rows.len())),
            other => lines.push(format!("{other:?}").split_whitespace().next().unwrap_or("").to_string()),
        }
        Ok(QueryResult { columns: vec!["plan".into()], rows: lines.into_iter().map(|l| vec![Value::Text(l)]).collect(), message: None })
    }

    /// Dot-commands for the shell and the web demo.
    pub fn meta(&mut self, cmd: &str) -> Result<String, String> {
        let mut parts = cmd.split_whitespace();
        match parts.next() {
            Some(".tables") => Ok(self.tables.keys().cloned().collect::<Vec<_>>().join("  ")),
            Some(".schema") => {
                let want = parts.next();
                Ok(self.tables.values().filter(|t| want.map_or(true, |w| w == t.name)).map(Table::ddl).collect::<Vec<_>>().join("\n"))
            }
            Some(".stats") => {
                let mut out = vec![format!("pages: {} ({} KB), page reads: {}, page writes: {}", self.pager.page_count(), self.pager.page_count() as usize * PAGE_SIZE / 1024, self.pager.reads, self.pager.writes)];
                let tables: Vec<Table> = self.tables.values().cloned().collect();
                for t in tables {
                    let (depth, pages) = btree::stats(&mut self.pager, t.root).map_err(|e| e.to_string())?;
                    let rows = self.scan(&t)?.len();
                    out.push(format!("{}: {rows} rows, b+tree depth {depth}, {pages} pages, {} index(es)", t.name, t.indexes.len()));
                }
                Ok(out.join("\n"))
            }
            _ => Err("unknown command; try .tables, .schema [table], .stats".into()),
        }
    }
}


fn table_scope(t: &Table, binding: &str) -> Scope {
    Scope { cols: t.columns.iter().map(|c| (binding.to_string(), c.name.clone())).collect() }
}

fn conjuncts(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Binary(a, BinOp::And, b) => {
            let mut v = conjuncts(a);
            v.extend(conjuncts(b));
            v
        }
        other => vec![other],
    }
}

/// Picks the cheapest way to find candidate rows from the WHERE clause.
/// The full filter is still applied afterwards, so this only affects speed.
fn plan_access(t: &Table, binding: &str, filter: Option<&Expr>, scope: &Scope) -> Access {
    let Some(f) = filter else { return Access::Scan };
    let col_of = |e: &Expr| -> Option<usize> {
        match e {
            Expr::Col(q, n) if q.as_deref().map_or(true, |q| q == binding) => scope.resolve(q, n).ok(),
            _ => None,
        }
    };
    let constant = |e: &Expr| -> Option<Value> {
        match e {
            Expr::Lit(v) if !v.is_null() => Some(v.clone()),
            _ => None,
        }
    };
    let index_on = |c: usize| t.indexes.iter().filter(|i| i.column == c).max_by_key(|i| i.unique);
    let mut best: Option<(u8, Access)> = None;
    let mut consider = |score: u8, a: Access| {
        if best.as_ref().map_or(true, |(s, _)| score > *s) {
            best = Some((score, a));
        }
    };
    for c in conjuncts(f) {
        match c {
            Expr::Binary(a, op, b) => {
                let (col, val, op) = match (col_of(a), constant(b), col_of(b), constant(a)) {
                    (Some(c), Some(v), _, _) => (c, v, *op),
                    (_, _, Some(c), Some(v)) => (
                        c,
                        v,
                        match op {
                            BinOp::Lt => BinOp::Gt,
                            BinOp::Le => BinOp::Ge,
                            BinOp::Gt => BinOp::Lt,
                            BinOp::Ge => BinOp::Le,
                            o => *o,
                        },
                    ),
                    _ => continue,
                };
                let is_rowid = Some(col) == t.rowid_col;
                match (op, is_rowid) {
                    (BinOp::Eq, true) => {
                        if let Value::Int(i) = val.clone().coerce(Type::Integer).unwrap_or(Value::Null) {
                            consider(10, Access::Rowids(vec![i]));
                        }
                    }
                    (BinOp::Eq, false) => {
                        if let Some(i) = index_on(col) {
                            let v = val.coerce(t.columns[col].ty).unwrap_or(Value::Null);
                            consider(if i.unique { 9 } else { 7 }, Access::IndexEq(i.name.clone(), vec![v]));
                        }
                    }
                    (BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, true) => {
                        if let Some(n) = val.as_f64() {
                            let (lo, hi) = match op {
                                BinOp::Gt => ((n.floor() as i64).saturating_add(1), i64::MAX),
                                BinOp::Ge => (n.ceil() as i64, i64::MAX),
                                BinOp::Lt => (i64::MIN, (n.ceil() as i64).saturating_sub(1)),
                                _ => (i64::MIN, n.floor() as i64),
                            };
                            consider(5, Access::RowidRange(lo, hi));
                        }
                    }
                    (BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, false) => {
                        if let Some(i) = index_on(col) {
                            let v = val.coerce(t.columns[col].ty).unwrap_or(Value::Null);
                            let (lo, hi) = match op {
                                BinOp::Gt => (Bound::Excluded(v), Bound::Unbounded),
                                BinOp::Ge => (Bound::Included(v), Bound::Unbounded),
                                BinOp::Lt => (Bound::Excluded(Value::Null), Bound::Excluded(v)),
                                _ => (Bound::Excluded(Value::Null), Bound::Included(v)),
                            };
                            consider(4, Access::IndexRange(i.name.clone(), lo, hi));
                        }
                    }
                    _ => {}
                }
            }
            Expr::InList(e, items, false) => {
                let Some(col) = col_of(e) else { continue };
                let vals: Option<Vec<Value>> = items.iter().map(constant).collect();
                let Some(vals) = vals else { continue };
                if Some(col) == t.rowid_col {
                    let ids: Vec<i64> = vals.iter().filter_map(|v| match v.clone().coerce(Type::Integer) {
                        Ok(Value::Int(i)) => Some(i),
                        _ => None,
                    }).collect();
                    consider(8, Access::Rowids(ids));
                } else if let Some(i) = index_on(col) {
                    consider(6, Access::IndexEq(i.name.clone(), vals.into_iter().map(|v| v.coerce(t.columns[col].ty).unwrap_or(Value::Null)).collect()));
                }
            }
            Expr::Between(e, lo, hi, false) => {
                let (Some(col), Some(lo), Some(hi)) = (col_of(e), constant(lo), constant(hi)) else { continue };
                if Some(col) == t.rowid_col {
                    if let (Some(a), Some(b)) = (lo.as_f64(), hi.as_f64()) {
                        consider(5, Access::RowidRange(a.ceil() as i64, b.floor() as i64));
                    }
                } else if let Some(i) = index_on(col) {
                    consider(4, Access::IndexRange(i.name.clone(), Bound::Included(lo), Bound::Included(hi)));
                }
            }
            _ => {}
        }
    }
    best.map(|b| b.1).unwrap_or(Access::Scan)
}

/// For `JOIN r ON r.col = <expr over left>` where r.col is the rowid or indexed,
/// returns (r.col, left-side key expression) so the join can use lookups.
fn join_lookup(rt: &Table, binding: &str, on: &Expr, left: &Scope) -> Option<(usize, Expr)> {
    for c in conjuncts(on) {
        let Expr::Binary(a, BinOp::Eq, b) = c else { continue };
        for (r, l) in [(a, b), (b, a)] {
            if let Expr::Col(Some(q), name) = &**r {
                if q == binding {
                    if let Some(col) = rt.columns.iter().position(|c| &c.name == name) {
                        let usable = Some(col) == rt.rowid_col || rt.indexes.iter().any(|i| i.column == col);
                        let left_only = refs_resolve(l, left);
                        if usable && left_only {
                            return Some((col, (**l).clone()));
                        }
                    }
                }
            }
        }
    }
    None
}

fn refs_resolve(e: &Expr, scope: &Scope) -> bool {
    match e {
        Expr::Col(q, n) => scope.resolve(q, n).is_ok(),
        Expr::Lit(_) => true,
        Expr::Binary(a, _, b) => refs_resolve(a, scope) && refs_resolve(b, scope),
        Expr::Neg(a) | Expr::Not(a) => refs_resolve(a, scope),
        _ => false,
    }
}

fn like(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let (mut ti, mut pi) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '%')
}

fn arith(a: Value, op: BinOp, b: Value) -> Result<Value, String> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    if op == BinOp::Concat {
        return Ok(Value::Text(format!("{a}{b}")));
    }
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        return Ok(match op {
            BinOp::Add => x.checked_add(*y).map(Value::Int).unwrap_or(Value::Real(*x as f64 + *y as f64)),
            BinOp::Sub => x.checked_sub(*y).map(Value::Int).unwrap_or(Value::Real(*x as f64 - *y as f64)),
            BinOp::Mul => x.checked_mul(*y).map(Value::Int).unwrap_or(Value::Real(*x as f64 * *y as f64)),
            BinOp::Div if *y == 0 => Value::Null,
            BinOp::Div => Value::Int(x / y),
            BinOp::Mod if *y == 0 => Value::Null,
            BinOp::Mod => Value::Int(x % y),
            _ => unreachable!(),
        });
    }
    let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else { return Err(format!("cannot apply arithmetic to {a} and {b}")) };
    Ok(match op {
        BinOp::Add => Value::Real(x + y),
        BinOp::Sub => Value::Real(x - y),
        BinOp::Mul => Value::Real(x * y),
        BinOp::Div if y == 0.0 => Value::Null,
        BinOp::Div => Value::Real(x / y),
        BinOp::Mod if y == 0.0 => Value::Null,
        BinOp::Mod => Value::Real(x % y),
        _ => unreachable!(),
    })
}

fn compare(a: &Value, op: BinOp, b: &Value) -> Value {
    if a.is_null() || b.is_null() {
        return Value::Null;
    }
    let o = a.cmp(b);
    use std::cmp::Ordering::*;
    Value::Bool(match op {
        BinOp::Eq => o == Equal,
        BinOp::Ne => o != Equal,
        BinOp::Lt => o == Less,
        BinOp::Le => o != Greater,
        BinOp::Gt => o == Greater,
        BinOp::Ge => o != Less,
        _ => unreachable!(),
    })
}

/// Evaluates an expression for one row. When `group` is given, aggregate functions
/// are computed over the group's rows; otherwise they are an error.
fn eval(e: &Expr, scope: &Scope, row: &[Value], group: Option<&[Row]>) -> Result<Value, String> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Col(q, n) => row.get(scope.resolve(q, n)?).cloned().unwrap_or(Value::Null),
        Expr::Neg(x) => match eval(x, scope, row, group)? {
            Value::Int(i) => Value::Int(-i),
            Value::Real(r) => Value::Real(-r),
            Value::Null => Value::Null,
            v => return Err(format!("cannot negate {v}")),
        },
        Expr::Not(x) => match eval(x, scope, row, group)? {
            Value::Null => Value::Null,
            v => Value::Bool(!v.truthy()),
        },
        Expr::Binary(a, BinOp::And, b) => {
            let l = eval(a, scope, row, group)?;
            if !l.is_null() && !l.truthy() {
                return Ok(Value::Bool(false));
            }
            let r = eval(b, scope, row, group)?;
            match (l.is_null(), r.is_null()) {
                (_, false) if !r.truthy() => Value::Bool(false),
                (false, false) => Value::Bool(true),
                _ => Value::Null,
            }
        }
        Expr::Binary(a, BinOp::Or, b) => {
            let l = eval(a, scope, row, group)?;
            if l.truthy() {
                return Ok(Value::Bool(true));
            }
            let r = eval(b, scope, row, group)?;
            if r.truthy() {
                Value::Bool(true)
            } else if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Bool(false)
            }
        }
        Expr::Binary(a, op @ (BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), b) => {
            compare(&eval(a, scope, row, group)?, *op, &eval(b, scope, row, group)?)
        }
        Expr::Binary(a, op, b) => arith(eval(a, scope, row, group)?, *op, eval(b, scope, row, group)?)?,
        Expr::IsNull(x, neg) => Value::Bool(eval(x, scope, row, group)?.is_null() != *neg),
        Expr::InList(x, items, neg) => {
            let v = eval(x, scope, row, group)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            let mut found = false;
            for i in items {
                if eval(i, scope, row, group)? == v {
                    found = true;
                    break;
                }
            }
            Value::Bool(found != *neg)
        }
        Expr::Between(x, lo, hi, neg) => {
            let v = eval(x, scope, row, group)?;
            let (l, h) = (eval(lo, scope, row, group)?, eval(hi, scope, row, group)?);
            if v.is_null() || l.is_null() || h.is_null() {
                return Ok(Value::Null);
            }
            Value::Bool((v >= l && v <= h) != *neg)
        }
        Expr::Like(x, p, neg) => {
            let (v, p) = (eval(x, scope, row, group)?, eval(p, scope, row, group)?);
            if v.is_null() || p.is_null() {
                return Ok(Value::Null);
            }
            Value::Bool(like(&v.to_string(), &p.to_string()) != *neg)
        }
        Expr::CountStar => match group {
            Some(g) => Value::Int(g.len() as i64),
            None => return Err("aggregate COUNT(*) used outside of an aggregate query".into()),
        },
        Expr::Func(name, args, distinct) if AGGREGATES.contains(&name.as_str()) => {
            let Some(g) = group else { return Err(format!("aggregate {name}() is not allowed here")) };
            let arg = args.first().ok_or_else(|| format!("{name}() needs an argument"))?;
            let mut vals: Vec<Value> = Vec::new();
            for r in g {
                let v = eval(arg, scope, r, None)?;
                if !v.is_null() {
                    vals.push(v);
                }
            }
            if *distinct {
                let set: BTreeSet<Value> = vals.into_iter().collect();
                vals = set.into_iter().collect();
            }
            match name.as_str() {
                "COUNT" => Value::Int(vals.len() as i64),
                "MIN" => vals.into_iter().min().unwrap_or(Value::Null),
                "MAX" => vals.into_iter().max().unwrap_or(Value::Null),
                "SUM" | "AVG" => {
                    if vals.is_empty() {
                        return Ok(Value::Null);
                    }
                    let all_int = vals.iter().all(|v| matches!(v, Value::Int(_)));
                    let n = vals.len();
                    if name == "SUM" && all_int {
                        Value::Int(vals.iter().map(|v| if let Value::Int(i) = v { *i } else { 0 }).fold(0i64, |a, b| a.saturating_add(b)))
                    } else {
                        let s: f64 = vals.iter().map(|v| v.as_f64().ok_or_else(|| format!("{name}() of non-numeric value {v}"))).sum::<Result<f64, String>>()?;
                        Value::Real(if name == "AVG" { s / n as f64 } else { s })
                    }
                }
                _ => unreachable!(),
            }
        }
        Expr::Func(name, args, _) => {
            let vals: Vec<Value> = args.iter().map(|a| eval(a, scope, row, group)).collect::<Result<_, _>>()?;
            let a0 = vals.first().cloned().unwrap_or(Value::Null);
            let need = |n: usize| if vals.len() < n { Err(format!("{name}() expects {n} argument(s)")) } else { Ok(()) };
            match name.as_str() {
                "UPPER" | "LOWER" | "LENGTH" | "ABS" | "TRIM" => {
                    need(1)?;
                    if a0.is_null() {
                        return Ok(Value::Null);
                    }
                    match name.as_str() {
                        "UPPER" => Value::Text(a0.to_string().to_uppercase()),
                        "LOWER" => Value::Text(a0.to_string().to_lowercase()),
                        "TRIM" => Value::Text(a0.to_string().trim().to_string()),
                        "LENGTH" => Value::Int(a0.to_string().chars().count() as i64),
                        _ => match a0 {
                            Value::Int(i) => Value::Int(i.abs()),
                            v => Value::Real(v.as_f64().ok_or("ABS() of non-numeric value")?.abs()),
                        },
                    }
                }
                "ROUND" => {
                    need(1)?;
                    let digits = match vals.get(1) {
                        Some(Value::Int(d)) => *d as i32,
                        _ => 0,
                    };
                    match a0.as_f64() {
                        None => Value::Null,
                        Some(f) => {
                            let m = 10f64.powi(digits);
                            Value::Real((f * m).round() / m)
                        }
                    }
                }
                "COALESCE" | "IFNULL" => vals.into_iter().find(|v| !v.is_null()).unwrap_or(Value::Null),
                "SUBSTR" => {
                    need(2)?;
                    if a0.is_null() {
                        return Ok(Value::Null);
                    }
                    let s: Vec<char> = a0.to_string().chars().collect();
                    let start = match &vals[1] {
                        Value::Int(i) => (*i - 1).max(0) as usize,
                        _ => 0,
                    };
                    let len = match vals.get(2) {
                        Some(Value::Int(l)) => (*l).max(0) as usize,
                        _ => usize::MAX,
                    };
                    Value::Text(s.iter().skip(start).take(len).collect())
                }
                _ => return Err(format!("no such function: {name}")),
            }
        }
    })
}

/// Renders a result as an ASCII table (shell and web demo).
pub fn format_result(r: &QueryResult) -> String {
    if r.columns.is_empty() {
        return r.message.clone().unwrap_or_default();
    }
    let cells: Vec<Vec<String>> = r.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect()).collect();
    let widths: Vec<usize> = r.columns.iter().enumerate().map(|(i, c)| cells.iter().map(|row| row[i].chars().count()).chain([c.chars().count()]).max().unwrap_or(0)).collect();
    let line = format!("+{}+", widths.iter().map(|w| "-".repeat(w + 2)).collect::<Vec<_>>().join("+"));
    let fmt_row = |vals: &[String], numeric: &[bool]| {
        let parts: Vec<String> = vals
            .iter()
            .zip(&widths)
            .zip(numeric)
            .map(|((v, w), &n)| if n { format!(" {v:>w$} ") } else { format!(" {v:<w$} ") })
            .collect();
        format!("|{}|", parts.join("|"))
    };
    let numeric: Vec<bool> = (0..r.columns.len()).map(|i| r.rows.iter().all(|row| matches!(row[i], Value::Int(_) | Value::Real(_) | Value::Null)) && !r.rows.is_empty()).collect();
    let mut out = vec![line.clone(), fmt_row(&r.columns, &vec![false; r.columns.len()]), line.clone()];
    for row in &cells {
        out.push(fmt_row(row, &numeric));
    }
    out.push(line);
    out.push(format!("{} row{}", r.rows.len(), if r.rows.len() == 1 { "" } else { "s" }));
    out.join("\n")
}
