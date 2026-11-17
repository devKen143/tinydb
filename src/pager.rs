//! Fixed-size pages on a storage backend, a page cache, and a rollback journal.
//!
//! Writes only touch the in-memory cache until `commit`. Commit is atomic:
//!   1. write the *original* image of every dirty page to the journal, fsync
//!   2. write the dirty pages into the database file, fsync
//!   3. delete the journal
//! If we crash during (2), the journal still exists on the next open and the
//! original pages are copied back, so the file is never half-committed.

use std::collections::{HashMap, HashSet};
use std::io;

pub const PAGE_SIZE: usize = 4096;
pub type PageId = u32;
pub type Page = Vec<u8>;

/// Where pages physically live: a file on disk or memory (tests, WebAssembly).
pub trait Storage {
    fn read_page(&mut self, id: PageId) -> io::Result<Option<Page>>;
    fn write_page(&mut self, id: PageId, data: &[u8]) -> io::Result<()>;
    fn page_count(&mut self) -> io::Result<u32>;
    fn sync(&mut self) -> io::Result<()>;
    /// Journal operations (rollback journal of original page images).
    fn journal_write(&mut self, entries: &[(PageId, Page)], page_count: u32) -> io::Result<()>;
    fn journal_read(&mut self) -> io::Result<Option<(Vec<(PageId, Page)>, u32)>>;
    fn journal_clear(&mut self) -> io::Result<()>;
    fn truncate(&mut self, pages: u32) -> io::Result<()>;
}

#[derive(Default)]
pub struct MemStorage {
    pages: Vec<Page>,
    journal: Option<(Vec<(PageId, Page)>, u32)>,
}

impl Storage for MemStorage {
    fn read_page(&mut self, id: PageId) -> io::Result<Option<Page>> {
        Ok(self.pages.get(id as usize).cloned())
    }
    fn write_page(&mut self, id: PageId, data: &[u8]) -> io::Result<()> {
        let i = id as usize;
        if self.pages.len() <= i {
            self.pages.resize(i + 1, vec![0; PAGE_SIZE]);
        }
        self.pages[i] = data.to_vec();
        Ok(())
    }
    fn page_count(&mut self) -> io::Result<u32> {
        Ok(self.pages.len() as u32)
    }
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn journal_write(&mut self, entries: &[(PageId, Page)], page_count: u32) -> io::Result<()> {
        self.journal = Some((entries.to_vec(), page_count));
        Ok(())
    }
    fn journal_read(&mut self) -> io::Result<Option<(Vec<(PageId, Page)>, u32)>> {
        Ok(self.journal.clone())
    }
    fn journal_clear(&mut self) -> io::Result<()> {
        self.journal = None;
        Ok(())
    }
    fn truncate(&mut self, pages: u32) -> io::Result<()> {
        self.pages.truncate(pages as usize);
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use file::FileStorage;

#[cfg(not(target_arch = "wasm32"))]
mod file {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;

    pub struct FileStorage {
        file: File,
        journal_path: PathBuf,
    }

    impl FileStorage {
        pub fn open(path: &std::path::Path) -> io::Result<Self> {
            let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
            let mut jp = path.as_os_str().to_owned();
            jp.push("-journal");
            Ok(FileStorage { file, journal_path: PathBuf::from(jp) })
        }
    }

    impl Storage for FileStorage {
        fn read_page(&mut self, id: PageId) -> io::Result<Option<Page>> {
            if id >= self.page_count()? {
                return Ok(None);
            }
            let mut buf = vec![0u8; PAGE_SIZE];
            self.file.seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
            self.file.read_exact(&mut buf)?;
            Ok(Some(buf))
        }
        fn write_page(&mut self, id: PageId, data: &[u8]) -> io::Result<()> {
            self.file.seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
            self.file.write_all(data)
        }
        fn page_count(&mut self) -> io::Result<u32> {
            Ok((self.file.metadata()?.len() / PAGE_SIZE as u64) as u32)
        }
        fn sync(&mut self) -> io::Result<()> {
            self.file.sync_all()
        }
        fn journal_write(&mut self, entries: &[(PageId, Page)], page_count: u32) -> io::Result<()> {
            let mut out = Vec::with_capacity(16 + entries.len() * (PAGE_SIZE + 4));
            out.extend_from_slice(b"TJRNL001");
            out.extend_from_slice(&page_count.to_le_bytes());
            out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            for (id, p) in entries {
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(p);
            }
            // a checksum guards against a torn journal (which is then ignored: the db was not touched yet)
            let sum = out.iter().fold(0u32, |a, &b| a.rotate_left(5) ^ b as u32);
            out.extend_from_slice(&sum.to_le_bytes());
            let mut f = File::create(&self.journal_path)?;
            f.write_all(&out)?;
            f.sync_all()
        }
        fn journal_read(&mut self) -> io::Result<Option<(Vec<(PageId, Page)>, u32)>> {
            let Ok(data) = std::fs::read(&self.journal_path) else { return Ok(None) };
            if data.len() < 20 || &data[..8] != b"TJRNL001" {
                return Ok(None);
            }
            let (body, sum) = data.split_at(data.len() - 4);
            if body.iter().fold(0u32, |a, &b| a.rotate_left(5) ^ b as u32) != u32::from_le_bytes(sum.try_into().unwrap()) {
                return Ok(None);
            }
            let pc = u32::from_le_bytes(body[8..12].try_into().unwrap());
            let n = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
            let mut entries = Vec::with_capacity(n);
            let mut pos = 16;
            for _ in 0..n {
                let id = u32::from_le_bytes(body[pos..pos + 4].try_into().unwrap());
                entries.push((id, body[pos + 4..pos + 4 + PAGE_SIZE].to_vec()));
                pos += 4 + PAGE_SIZE;
            }
            Ok(Some((entries, pc)))
        }
        fn journal_clear(&mut self) -> io::Result<()> {
            match std::fs::remove_file(&self.journal_path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
        fn truncate(&mut self, pages: u32) -> io::Result<()> {
            self.file.set_len(pages as u64 * PAGE_SIZE as u64)
        }
    }
}

pub struct Pager {
    storage: Box<dyn Storage>,
    cache: HashMap<PageId, Page>,
    dirty: HashSet<PageId>,
    committed_pages: u32,
    page_count: u32,
    pub reads: u64,
    pub writes: u64,
}

impl Pager {
    pub fn open(mut storage: Box<dyn Storage>) -> io::Result<Self> {
        // crash recovery: a surviving journal means a commit was interrupted
        if let Some((entries, page_count)) = storage.journal_read()? {
            for (id, page) in &entries {
                storage.write_page(*id, page)?;
            }
            storage.truncate(page_count)?;
            storage.sync()?;
        }
        storage.journal_clear()?;
        let pc = storage.page_count()?;
        Ok(Pager { storage, cache: HashMap::new(), dirty: HashSet::new(), committed_pages: pc, page_count: pc, reads: 0, writes: 0 })
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn get(&mut self, id: PageId) -> io::Result<&Page> {
        if !self.cache.contains_key(&id) {
            self.reads += 1;
            let p = self.storage.read_page(id)?.unwrap_or_else(|| vec![0; PAGE_SIZE]);
            self.cache.insert(id, p);
        }
        Ok(&self.cache[&id])
    }

    pub fn write(&mut self, id: PageId, data: Page) -> io::Result<()> {
        debug_assert_eq!(data.len(), PAGE_SIZE);
        self.get(id)?; // make sure the original is cached for the journal
        self.dirty.insert(id);
        self.cache.insert(id, data);
        Ok(())
    }

    pub fn allocate(&mut self) -> io::Result<PageId> {
        let id = self.page_count;
        self.page_count += 1;
        self.cache.insert(id, vec![0; PAGE_SIZE]);
        self.dirty.insert(id);
        Ok(id)
    }

    pub fn has_changes(&self) -> bool {
        !self.dirty.is_empty()
    }

    pub fn commit(&mut self) -> io::Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let mut originals = Vec::new();
        for &id in &self.dirty {
            if id < self.committed_pages {
                if let Some(p) = self.storage.read_page(id)? {
                    originals.push((id, p));
                }
            }
        }
        self.storage.journal_write(&originals, self.committed_pages)?;
        let mut ids: Vec<PageId> = self.dirty.iter().copied().collect();
        ids.sort_unstable();
        for id in ids {
            self.storage.write_page(id, &self.cache[&id])?;
            self.writes += 1;
        }
        self.storage.sync()?;
        self.storage.journal_clear()?;
        self.dirty.clear();
        self.committed_pages = self.page_count;
        Ok(())
    }

    pub fn rollback(&mut self) {
        for id in self.dirty.drain() {
            self.cache.remove(&id);
        }
        self.page_count = self.committed_pages;
    }

    /// Test hook: performs a commit but "crashes" after writing half the pages.
    #[doc(hidden)]
    pub fn commit_and_crash_midway(&mut self) -> io::Result<()> {
        let mut originals = Vec::new();
        for &id in &self.dirty {
            if id < self.committed_pages {
                if let Some(p) = self.storage.read_page(id)? {
                    originals.push((id, p));
                }
            }
        }
        self.storage.journal_write(&originals, self.committed_pages)?;
        let mut ids: Vec<PageId> = self.dirty.iter().copied().collect();
        ids.sort_unstable();
        for id in ids.iter().take(ids.len() / 2 + 1) {
            self.storage.write_page(*id, &self.cache[id])?;
        }
        self.storage.sync()
    }

    pub fn into_storage(self) -> Box<dyn Storage> {
        self.storage
    }
}
