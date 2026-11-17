//! SQL front end: tokens, syntax tree and parser.
pub mod lexer;
pub mod parser;

use crate::value::{Type, Value};

#[derive(Debug, Clone)]
pub enum Stmt {
    CreateTable { name: String, columns: Vec<ColumnDef>, if_not_exists: bool },
    DropTable { name: String, if_exists: bool },
    CreateIndex { name: String, table: String, column: String, unique: bool },
    DropIndex { name: String },
    Insert { table: String, columns: Option<Vec<String>>, rows: Vec<Vec<Expr>> },
    Select(Box<Select>),
    Update { table: String, sets: Vec<(String, Expr)>, filter: Option<Expr> },
    Delete { table: String, filter: Option<Expr> },
    Begin,
    Commit,
    Rollback,
    Explain(Box<Stmt>),
}

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub ty: Type,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
    pub default: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Option<TableRef>,
    pub joins: Vec<Join>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone)]
pub enum SelectItem {
    Star,
    TableStar(String),
    Expr(Expr, Option<String>),
}

#[derive(Debug, Clone)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
}

impl TableRef {
    pub fn binding(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone)]
pub struct Join {
    pub table: TableRef,
    pub on: Expr,
    pub left: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Value),
    Col(Option<String>, String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    IsNull(Box<Expr>, bool),
    InList(Box<Expr>, Vec<Expr>, bool),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    Like(Box<Expr>, Box<Expr>, bool),
    Func(String, Vec<Expr>, bool),
    CountStar,
}

pub const AGGREGATES: &[&str] = &["COUNT", "SUM", "AVG", "MIN", "MAX"];

impl Expr {
    pub fn has_aggregate(&self) -> bool {
        match self {
            Expr::CountStar => true,
            Expr::Func(n, args, _) => AGGREGATES.contains(&n.as_str()) || args.iter().any(Expr::has_aggregate),
            Expr::Neg(e) | Expr::Not(e) | Expr::IsNull(e, _) => e.has_aggregate(),
            Expr::Binary(a, _, b) | Expr::Like(a, b, _) => a.has_aggregate() || b.has_aggregate(),
            Expr::Between(a, b, c, _) => a.has_aggregate() || b.has_aggregate() || c.has_aggregate(),
            Expr::InList(a, l, _) => a.has_aggregate() || l.iter().any(Expr::has_aggregate),
            Expr::Lit(_) | Expr::Col(..) => false,
        }
    }

    /// A readable rendering used for result column headers.
    pub fn label(&self) -> String {
        match self {
            Expr::Lit(Value::Text(s)) => format!("'{s}'"),
            Expr::Lit(v) => v.to_string(),
            Expr::Col(Some(t), c) => format!("{t}.{c}"),
            Expr::Col(None, c) => c.clone(),
            Expr::Neg(e) => format!("-{}", e.label()),
            Expr::Not(e) => format!("NOT {}", e.label()),
            Expr::Binary(a, op, b) => {
                let o = match op {
                    BinOp::Eq => "=",
                    BinOp::Ne => "<>",
                    BinOp::Lt => "<",
                    BinOp::Le => "<=",
                    BinOp::Gt => ">",
                    BinOp::Ge => ">=",
                    BinOp::And => "AND",
                    BinOp::Or => "OR",
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Mul => "*",
                    BinOp::Div => "/",
                    BinOp::Mod => "%",
                    BinOp::Concat => "||",
                };
                format!("{} {o} {}", a.label(), b.label())
            }
            Expr::IsNull(e, neg) => format!("{} IS {}NULL", e.label(), if *neg { "NOT " } else { "" }),
            Expr::InList(e, _, neg) => format!("{} {}IN (...)", e.label(), if *neg { "NOT " } else { "" }),
            Expr::Between(e, ..) => format!("{} BETWEEN ...", e.label()),
            Expr::Like(a, b, _) => format!("{} LIKE {}", a.label(), b.label()),
            Expr::Func(n, args, d) => format!("{n}({}{})", if *d { "DISTINCT " } else { "" }, args.iter().map(Expr::label).collect::<Vec<_>>().join(", ")),
            Expr::CountStar => "COUNT(*)".into(),
        }
    }
}
