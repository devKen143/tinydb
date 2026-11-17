//! A B+tree keyed by i64 (the rowid) with byte-string values, one node per page.
//!
//! Leaf page:     [1][count u16][next leaf u32] then cells [key i64][len u16][value]
//! Interior page: [2][count u16][child0 u32]    then cells [key i64][child u32]
//! Child i+1 of an interior node holds keys >= key i. Leaves are chained for range scans.
//! Deletes do not rebalance (like many embedded engines, space is reclaimed on rewrite).

use crate::pager::{PageId, Pager, PAGE_SIZE};
use std::io;

/// Largest value that is guaranteed to fit (two cells per leaf after a split).
pub const MAX_VALUE: usize = (PAGE_SIZE - 7) / 2 - 10;

#[derive(Debug, Clone)]
enum Node {
    Leaf { keys: Vec<i64>, vals: Vec<Vec<u8>>, next: u32 },
    Interior { keys: Vec<i64>, children: Vec<u32> },
}

fn u16_at(p: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([p[i], p[i + 1]])
}
fn u32_at(p: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(p[i..i + 4].try_into().unwrap())
}
fn i64_at(p: &[u8], i: usize) -> i64 {
    i64::from_le_bytes(p[i..i + 8].try_into().unwrap())
}

impl Node {
    fn decode(p: &[u8]) -> io::Result<Node> {
        let count = u16_at(p, 1) as usize;
        match p[0] {
            1 => {
                let next = u32_at(p, 3);
                let (mut keys, mut vals) = (Vec::with_capacity(count), Vec::with_capacity(count));
                let mut pos = 7;
                for _ in 0..count {
                    keys.push(i64_at(p, pos));
                    let len = u16_at(p, pos + 8) as usize;
                    vals.push(p[pos + 10..pos + 10 + len].to_vec());
                    pos += 10 + len;
                }
                Ok(Node::Leaf { keys, vals, next })
            }
            2 => {
                let mut children = vec![u32_at(p, 3)];
                let mut keys = Vec::with_capacity(count);
                let mut pos = 7;
                for _ in 0..count {
                    keys.push(i64_at(p, pos));
                    children.push(u32_at(p, pos + 8));
                    pos += 12;
                }
                Ok(Node::Interior { keys, children })
            }
            t => Err(io::Error::new(io::ErrorKind::InvalidData, format!("corrupt b-tree page (type {t})"))),
        }
    }

    fn size(&self) -> usize {
        match self {
            Node::Leaf { vals, .. } => 7 + vals.iter().map(|v| 10 + v.len()).sum::<usize>(),
            Node::Interior { keys, .. } => 7 + keys.len() * 12,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut p = vec![0u8; PAGE_SIZE];
        match self {
            Node::Leaf { keys, vals, next } => {
                p[0] = 1;
                p[1..3].copy_from_slice(&(keys.len() as u16).to_le_bytes());
                p[3..7].copy_from_slice(&next.to_le_bytes());
                let mut pos = 7;
                for (k, v) in keys.iter().zip(vals) {
                    p[pos..pos + 8].copy_from_slice(&k.to_le_bytes());
                    p[pos + 8..pos + 10].copy_from_slice(&(v.len() as u16).to_le_bytes());
                    p[pos + 10..pos + 10 + v.len()].copy_from_slice(v);
                    pos += 10 + v.len();
                }
            }
            Node::Interior { keys, children } => {
                p[0] = 2;
                p[1..3].copy_from_slice(&(keys.len() as u16).to_le_bytes());
                p[3..7].copy_from_slice(&children[0].to_le_bytes());
                let mut pos = 7;
                for (k, c) in keys.iter().zip(&children[1..]) {
                    p[pos..pos + 8].copy_from_slice(&k.to_le_bytes());
                    p[pos + 8..pos + 12].copy_from_slice(&c.to_le_bytes());
                    pos += 12;
                }
            }
        }
        p
    }
}

fn load(pager: &mut Pager, id: PageId) -> io::Result<Node> {
    Node::decode(pager.get(id)?)
}

fn store(pager: &mut Pager, id: PageId, n: &Node) -> io::Result<()> {
    pager.write(id, n.encode())
}

pub fn create(pager: &mut Pager) -> io::Result<PageId> {
    let id = pager.allocate()?;
    store(pager, id, &Node::Leaf { keys: vec![], vals: vec![], next: 0 })?;
    Ok(id)
}

pub fn get(pager: &mut Pager, root: PageId, key: i64) -> io::Result<Option<Vec<u8>>> {
    let mut id = root;
    loop {
        match load(pager, id)? {
            Node::Interior { keys, children } => id = children[keys.partition_point(|&k| k <= key)],
            Node::Leaf { keys, vals, .. } => return Ok(keys.binary_search(&key).ok().map(|i| vals[i].clone())),
        }
    }
}

/// Inserts or replaces `key`. Returns true if the key was new.
pub fn insert(pager: &mut Pager, root: PageId, key: i64, val: &[u8]) -> io::Result<bool> {
    if val.len() > MAX_VALUE {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("row too large ({} bytes, max {MAX_VALUE})", val.len())));
    }
    let (new, split) = insert_rec(pager, root, key, val)?;
    if let Some((sep, right)) = split {
        // keep the root page id stable: move the old root's contents to a fresh page
        let left = pager.allocate()?;
        let old = pager.get(root)?.clone();
        pager.write(left, old)?;
        store(pager, root, &Node::Interior { keys: vec![sep], children: vec![left, right] })?;
    }
    Ok(new)
}

type Split = Option<(i64, PageId)>;

fn insert_rec(pager: &mut Pager, id: PageId, key: i64, val: &[u8]) -> io::Result<(bool, Split)> {
    match load(pager, id)? {
        Node::Leaf { mut keys, mut vals, next } => {
            let new = match keys.binary_search(&key) {
                Ok(i) => {
                    vals[i] = val.to_vec();
                    false
                }
                Err(i) => {
                    keys.insert(i, key);
                    vals.insert(i, val.to_vec());
                    true
                }
            };
            let node = Node::Leaf { keys, vals, next };
            if node.size() <= PAGE_SIZE {
                store(pager, id, &node)?;
                return Ok((new, None));
            }
            let Node::Leaf { mut keys, mut vals, next } = node else { unreachable!() };
            // split by bytes so both halves fit even with uneven row sizes
            let total: usize = vals.iter().map(|v| v.len() + 10).sum();
            let mut acc = 0;
            let mut mid = 0;
            while mid < keys.len() - 1 && acc + vals[mid].len() + 10 <= total / 2 {
                acc += vals[mid].len() + 10;
                mid += 1;
            }
            let mid = mid.max(1);
            let rkeys = keys.split_off(mid);
            let rvals = vals.split_off(mid);
            let right_id = pager.allocate()?;
            let sep = rkeys[0];
            store(pager, right_id, &Node::Leaf { keys: rkeys, vals: rvals, next })?;
            store(pager, id, &Node::Leaf { keys, vals, next: right_id })?;
            Ok((new, Some((sep, right_id))))
        }
        Node::Interior { mut keys, mut children } => {
            let idx = keys.partition_point(|&k| k <= key);
            let (new, split) = insert_rec(pager, children[idx], key, val)?;
            let Some((sep, right)) = split else { return Ok((new, None)) };
            keys.insert(idx, sep);
            children.insert(idx + 1, right);
            let node = Node::Interior { keys, children };
            if node.size() <= PAGE_SIZE {
                store(pager, id, &node)?;
                return Ok((new, None));
            }
            let Node::Interior { mut keys, mut children } = node else { unreachable!() };
            let mid = keys.len() / 2;
            let up = keys[mid];
            let rkeys = keys.split_off(mid + 1);
            keys.pop();
            let rchildren = children.split_off(mid + 1);
            let right_id = pager.allocate()?;
            store(pager, right_id, &Node::Interior { keys: rkeys, children: rchildren })?;
            store(pager, id, &Node::Interior { keys, children })?;
            Ok((new, Some((up, right_id))))
        }
    }
}

pub fn delete(pager: &mut Pager, root: PageId, key: i64) -> io::Result<bool> {
    let mut id = root;
    loop {
        match load(pager, id)? {
            Node::Interior { keys, children } => id = children[keys.partition_point(|&k| k <= key)],
            Node::Leaf { mut keys, mut vals, next } => {
                let Ok(i) = keys.binary_search(&key) else { return Ok(false) };
                keys.remove(i);
                vals.remove(i);
                store(pager, id, &Node::Leaf { keys, vals, next })?;
                return Ok(true);
            }
        }
    }
}

/// All (key, value) pairs with lo <= key <= hi, in key order, following the leaf chain.
pub fn range(pager: &mut Pager, root: PageId, lo: i64, hi: i64) -> io::Result<Vec<(i64, Vec<u8>)>> {
    let mut id = root;
    loop {
        match load(pager, id)? {
            Node::Interior { keys, children } => id = children[keys.partition_point(|&k| k <= lo)],
            Node::Leaf { .. } => break,
        }
    }
    let mut out = Vec::new();
    loop {
        let Node::Leaf { keys, vals, next } = load(pager, id)? else { unreachable!() };
        for (k, v) in keys.into_iter().zip(vals) {
            if k > hi {
                return Ok(out);
            }
            if k >= lo {
                out.push((k, v));
            }
        }
        if next == 0 {
            return Ok(out);
        }
        id = next;
    }
}

pub fn scan(pager: &mut Pager, root: PageId) -> io::Result<Vec<(i64, Vec<u8>)>> {
    range(pager, root, i64::MIN, i64::MAX)
}

/// Tree depth and page count (for EXPLAIN / .stats).
pub fn stats(pager: &mut Pager, root: PageId) -> io::Result<(usize, usize)> {
    fn walk(p: &mut Pager, id: PageId, depth: usize) -> io::Result<(usize, usize)> {
        match load(p, id)? {
            Node::Leaf { .. } => Ok((depth, 1)),
            Node::Interior { children, .. } => {
                let mut pages = 1;
                let mut d = depth;
                for c in children {
                    let (cd, cp) = walk(p, c, depth + 1)?;
                    d = d.max(cd);
                    pages += cp;
                }
                Ok((d, pages))
            }
        }
    }
    walk(pager, root, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::MemStorage;

    #[test]
    fn many_inserts_splits_and_scans() {
        let mut p = Pager::open(Box::new(MemStorage::default())).unwrap();
        p.allocate().unwrap(); // page 0 reserved for the header
        let root = create(&mut p).unwrap();
        // insert in a scrambled order with varied value sizes
        let n = 5000i64;
        for i in 0..n {
            let k = (i * 7919) % n;
            let v = vec![(k % 251) as u8; (k as usize * 13) % 300];
            assert!(insert(&mut p, root, k, &v).unwrap());
        }
        let (depth, pages) = stats(&mut p, root).unwrap();
        assert!(depth >= 2 && pages > 50, "depth {depth} pages {pages}");
        let all = scan(&mut p, root).unwrap();
        assert_eq!(all.len(), n as usize);
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(get(&mut p, root, 1234).unwrap().unwrap().len(), (1234 * 13) % 300);
        assert_eq!(range(&mut p, root, 100, 104).unwrap().iter().map(|e| e.0).collect::<Vec<_>>(), vec![100, 101, 102, 103, 104]);
        // replace and delete
        assert!(!insert(&mut p, root, 10, b"new").unwrap());
        assert_eq!(get(&mut p, root, 10).unwrap().unwrap(), b"new");
        for k in (0..n).step_by(2) {
            assert!(delete(&mut p, root, k).unwrap());
        }
        assert!(!delete(&mut p, root, 0).unwrap());
        assert_eq!(scan(&mut p, root).unwrap().len(), (n / 2) as usize);
        assert!(get(&mut p, root, 4).unwrap().is_none());
    }

    #[test]
    fn rejects_oversized_values() {
        let mut p = Pager::open(Box::new(MemStorage::default())).unwrap();
        p.allocate().unwrap();
        let root = create(&mut p).unwrap();
        assert!(insert(&mut p, root, 1, &vec![0; MAX_VALUE + 1]).is_err());
        assert!(insert(&mut p, root, 1, &vec![0; MAX_VALUE]).is_ok());
        assert!(insert(&mut p, root, 2, &vec![0; MAX_VALUE]).is_ok());
        assert!(insert(&mut p, root, 3, &vec![0; MAX_VALUE]).is_ok());
        assert_eq!(scan(&mut p, root).unwrap().len(), 3);
    }
}
