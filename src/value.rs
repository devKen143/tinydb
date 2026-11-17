//! SQL values, column types and the binary row / schema encodings.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Integer,
    Real,
    Text,
    Boolean,
}

impl Type {
    pub fn parse(s: &str) -> Option<Type> {
        Some(match s.to_ascii_uppercase().as_str() {
            "INT" | "INTEGER" | "BIGINT" => Type::Integer,
            "REAL" | "FLOAT" | "DOUBLE" => Type::Real,
            "TEXT" | "VARCHAR" | "STRING" => Type::Text,
            "BOOL" | "BOOLEAN" => Type::Boolean,
            _ => return None,
        })
    }
    pub fn name(&self) -> &'static str {
        match self {
            Type::Integer => "INTEGER",
            Type::Real => "REAL",
            Type::Text => "TEXT",
            Type::Boolean => "BOOLEAN",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Bool(bool),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Real(r) => *r != 0.0,
            Value::Text(_) | Value::Null => false,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Real(r) => Some(*r),
            Value::Bool(b) => Some(*b as i64 as f64),
            _ => None,
        }
    }
    /// Converts a value for storage in a column of type `t`.
    pub fn coerce(self, t: Type) -> Result<Value, String> {
        Ok(match (t, self) {
            (_, Value::Null) => Value::Null,
            (Type::Integer, Value::Int(i)) => Value::Int(i),
            (Type::Integer, Value::Real(r)) if r.fract() == 0.0 => Value::Int(r as i64),
            (Type::Integer, Value::Bool(b)) => Value::Int(b as i64),
            (Type::Integer, Value::Text(s)) => Value::Int(s.trim().parse().map_err(|_| format!("cannot store '{s}' in an INTEGER column"))?),
            (Type::Real, v) if v.as_f64().is_some() => Value::Real(v.as_f64().unwrap()),
            (Type::Real, Value::Text(s)) => Value::Real(s.trim().parse().map_err(|_| format!("cannot store '{s}' in a REAL column"))?),
            (Type::Text, Value::Text(s)) => Value::Text(s),
            (Type::Text, v) => Value::Text(v.to_string()),
            (Type::Boolean, Value::Bool(b)) => Value::Bool(b),
            (Type::Boolean, Value::Int(i)) => Value::Bool(i != 0),
            (Type::Boolean, Value::Text(s)) => match s.to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => Value::Bool(true),
                "false" | "f" | "0" => Value::Bool(false),
                _ => return Err(format!("cannot store '{s}' in a BOOLEAN column")),
            },
            (t, v) => return Err(format!("cannot store {v} in a {} column", t.name())),
        })
    }
    fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Real(_) => 2,
            Value::Text(_) => 3,
        }
    }
}

/// Total order used by ORDER BY, indexes and GROUP BY (NULLs first).
impl Ord for Value {
    fn cmp(&self, o: &Self) -> Ordering {
        match (self, o) {
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (a, b) if a.rank() == 2 && b.rank() == 2 => a.as_f64().unwrap().total_cmp(&b.as_f64().unwrap()),
            (a, b) => a.rank().cmp(&b.rank()),
        }
    }
}
impl PartialOrd for Value {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl PartialEq for Value {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Value {}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Real(r) => {
                if r.fract() == 0.0 && r.abs() < 1e15 {
                    write!(f, "{r:.1}")
                } else {
                    write!(f, "{r}")
                }
            }
            Value::Text(s) => write!(f, "{s}"),
            Value::Bool(b) => write!(f, "{}", if *b { "true" } else { "false" }),
        }
    }
}

// ------------------------------------------------------------------ encoding

pub struct Writer(pub Vec<u8>);
impl Writer {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v)
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes())
    }
    pub fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes())
    }
    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    pub fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.u8(0),
            Value::Int(i) => {
                self.u8(1);
                self.i64(*i)
            }
            Value::Real(r) => {
                self.u8(2);
                self.0.extend_from_slice(&r.to_le_bytes())
            }
            Value::Text(s) => {
                self.u8(3);
                self.str(s)
            }
            Value::Bool(b) => {
                self.u8(4);
                self.u8(*b as u8)
            }
        }
    }
}

pub struct Reader<'a>(pub &'a [u8], pub usize);
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let s = self.0.get(self.1..self.1 + n).ok_or("corrupt record")?;
        self.1 += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn str(&mut self) -> Result<String, String> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    pub fn value(&mut self) -> Result<Value, String> {
        Ok(match self.u8()? {
            0 => Value::Null,
            1 => Value::Int(self.i64()?),
            2 => Value::Real(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            3 => Value::Text(self.str()?),
            4 => Value::Bool(self.u8()? != 0),
            t => return Err(format!("corrupt value tag {t}")),
        })
    }
}

pub fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    w.u32(row.len() as u32);
    row.iter().for_each(|v| w.value(v));
    w.0
}

pub fn decode_row(b: &[u8]) -> Result<Vec<Value>, String> {
    let mut r = Reader(b, 0);
    let n = r.u32()? as usize;
    (0..n).map(|_| r.value()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_roundtrip_and_ordering() {
        let row = vec![Value::Int(-5), Value::Real(2.5), Value::Text("héllo".into()), Value::Bool(true), Value::Null];
        assert_eq!(decode_row(&encode_row(&row)).unwrap(), row);
        let mut vs = vec![Value::Text("b".into()), Value::Int(3), Value::Null, Value::Real(2.5), Value::Text("a".into())];
        vs.sort();
        assert_eq!(vs, vec![Value::Null, Value::Real(2.5), Value::Int(3), Value::Text("a".into()), Value::Text("b".into())]);
        assert_eq!(Value::Int(2), Value::Real(2.0));
    }

    #[test]
    fn coercion() {
        assert_eq!(Value::Text("42".into()).coerce(Type::Integer).unwrap(), Value::Int(42));
        assert!(Value::Text("x".into()).coerce(Type::Integer).is_err());
        assert_eq!(Value::Int(1).coerce(Type::Real).unwrap(), Value::Real(1.0));
        assert_eq!(Value::Int(7).coerce(Type::Text).unwrap(), Value::Text("7".into()));
    }
}
