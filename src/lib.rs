//! A small LSM-tree key-value store: a write-ahead log, a memtable, sorted tables on disk and
//! compaction. A write is durable once it returns, and [`sim::SimFs`] is how the tests check that
//! this holds wherever the process dies.
//!
//! ```
//! use lsm_lite::{Db, Options};
//!
//! # let dir = std::env::temp_dir().join(format!("lsm-lite-doc-{}", std::process::id()));
//! let mut db = Db::open_dir(&dir, Options::default())?;
//! db.put(b"apple", b"red")?;
//! db.put(b"banana", b"yellow")?;
//! db.delete(b"banana")?;
//! assert_eq!(db.get(b"apple")?, Some(b"red".to_vec()));
//! assert_eq!(db.get(b"banana")?, None);
//! assert_eq!(db.scan(b"a".to_vec()..b"c".to_vec())?, [(b"apple".to_vec(), b"red".to_vec())]);
//! # drop(db);
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok::<(), std::io::Error>(())
//! ```

mod fs;
pub mod sim;
mod sstable;
mod wal;

pub use fs::{Fs, StdFs};

use std::collections::BTreeMap;
use std::io;
use std::ops::RangeBounds;
use std::path::Path;

use sstable::Table;

/// Settings for a database.
#[derive(Clone, Debug)]
pub struct Options {
    /// The memtable goes to a table on disk once it holds about this many bytes.
    pub memtable_bytes: usize,
    /// All tables are merged into one once there are this many.
    pub compact_at: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_bytes: 4 << 20,
            compact_at: 4,
        }
    }
}

const MANIFEST: &str = "MANIFEST";
const MANIFEST_TMP: &str = "MANIFEST.tmp";

fn log_name(id: u64) -> String {
    format!("{id:06}.log")
}

fn table_name(id: u64) -> String {
    format!("{id:06}.sst")
}

fn file_id(name: &str) -> Option<u64> {
    let (id, ext) = name.split_once('.')?;
    if ext == "log" || ext == "sst" {
        id.parse().ok()
    } else {
        None
    }
}

/// A key-value store in one directory. Writes take `&mut self`; put it behind a `Mutex` to share it.
pub struct Db {
    fs: Box<dyn Fs>,
    opts: Options,
    mem: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    mem_bytes: usize,
    tables: Vec<Table>, // oldest first
    log: u64,
    next_id: u64,
    failed: bool,
}

impl Db {
    /// Opens or creates a database in a directory.
    pub fn open_dir(dir: impl AsRef<Path>, opts: Options) -> io::Result<Db> {
        Db::open(StdFs::new(dir.as_ref())?, opts)
    }

    /// Opens or creates a database on any [`Fs`], recovering what the last run wrote.
    pub fn open(fs: impl Fs + 'static, opts: Options) -> io::Result<Db> {
        let fs: Box<dyn Fs> = Box::new(fs);
        let manifest = match fs.read(MANIFEST) {
            Ok(buf) => Manifest::decode(&buf)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Manifest::default(),
            Err(e) => return Err(e),
        };
        let names = fs.list()?;

        // Logs from the manifest's current one on hold writes that aren't in a table yet.
        let mut logs: Vec<u64> = names
            .iter()
            .filter(|n| n.ends_with(".log"))
            .filter_map(|n| file_id(n))
            .filter(|&id| id >= manifest.log)
            .collect();
        logs.sort_unstable();
        let mut mem = BTreeMap::new();
        for id in logs {
            for record in wal::records(&fs.read(&log_name(id))?).0 {
                let (key, value) = wal::decode(record)?;
                mem.insert(key, value);
            }
        }

        let tables = manifest
            .tables
            .iter()
            .map(|&id| Table::open(&*fs, id))
            .collect::<io::Result<_>>()?;
        let next_id = names
            .iter()
            .filter_map(|n| file_id(n))
            .map(|id| id + 1)
            .fold(manifest.next_id, u64::max);
        let mut db = Db {
            fs,
            opts,
            mem,
            mem_bytes: 0,
            tables,
            log: 0,
            next_id,
            failed: false,
        };
        // Put what the logs held into a table and start a new log, so nothing is ever appended
        // after a log's torn end.
        db.flush()?;
        Ok(db)
    }

    /// Reads a key.
    pub fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        self.check()?;
        if let Some(value) = self.mem.get(key) {
            return Ok(value.clone());
        }
        for table in self.tables.iter().rev() {
            if let Some(value) = table.get(&*self.fs, key)? {
                return Ok(value);
            }
        }
        Ok(None)
    }

    /// Returns the keys and values in a range, in order.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.check()?;
        let range = (range.start_bound().cloned(), range.end_bound().cloned());
        let mut merged = BTreeMap::new();
        // ponytail: reads every table in full; seek with the index when tables get large.
        for table in &self.tables {
            for (key, value) in table.entries(&*self.fs)? {
                if range.contains(&key) {
                    merged.insert(key, value);
                }
            }
        }
        for (key, value) in self.mem.range(range) {
            merged.insert(key.clone(), value.clone());
        }
        Ok(merged
            .into_iter()
            .filter_map(|(key, value)| Some((key, value?)))
            .collect())
    }

    /// Sets a key. It's durable once this returns.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        self.write(key, Some(value))
    }

    /// Deletes a key. It's durable once this returns.
    pub fn delete(&mut self, key: &[u8]) -> io::Result<()> {
        self.write(key, None)
    }

    fn write(&mut self, key: &[u8], value: Option<&[u8]>) -> io::Result<()> {
        self.check()?;
        let result = self.write_logged(key, value);
        // After a failed write, what's in memory may not match the disk.
        self.failed |= result.is_err();
        result
    }

    fn write_logged(&mut self, key: &[u8], value: Option<&[u8]>) -> io::Result<()> {
        let log = log_name(self.log);
        self.fs
            .append(&log, &wal::frame(&wal::encode(key, value)))?;
        self.fs.sync(&log)?;
        self.mem_bytes += key.len() + value.map_or(0, <[u8]>::len) + 16;
        self.mem.insert(key.to_vec(), value.map(<[u8]>::to_vec));
        if self.mem_bytes >= self.opts.memtable_bytes {
            self.flush()?;
        }
        Ok(())
    }

    fn check(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "an earlier write failed; reopen the database",
            ));
        }
        Ok(())
    }

    fn take_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id - 1
    }

    /// Writes the memtable to a table and switches to a new log.
    fn flush(&mut self) -> io::Result<()> {
        if !self.mem.is_empty() {
            let id = self.take_id();
            let entries = self.mem.iter().map(|(k, v)| (k.as_slice(), v.as_deref()));
            self.tables.push(Table::write(&*self.fs, id, entries)?);
        }
        let log = self.take_id();
        self.fs.create(&log_name(log))?;
        // The manifest must never name a file whose own name could still be lost.
        self.fs.sync_dir()?;
        self.write_manifest(log)?;
        self.log = log;
        self.mem.clear();
        self.mem_bytes = 0;
        self.remove_unused()?;
        if self.tables.len() >= self.opts.compact_at {
            self.compact()?;
        }
        Ok(())
    }

    /// Merges every table into one.
    // ponytail: rewrites all data each time; leveled compaction when the data outgrows that.
    fn compact(&mut self) -> io::Result<()> {
        let mut merged = BTreeMap::new();
        for table in &self.tables {
            merged.extend(table.entries(&*self.fs)?);
        }
        // Nothing older is left for a deletion to hide.
        merged.retain(|_, value| value.is_some());
        let mut tables = Vec::new();
        if !merged.is_empty() {
            let id = self.take_id();
            let entries = merged.iter().map(|(k, v)| (k.as_slice(), v.as_deref()));
            tables.push(Table::write(&*self.fs, id, entries)?);
            self.fs.sync_dir()?;
        }
        self.tables = tables;
        self.write_manifest(self.log)?;
        self.remove_unused()
    }

    /// Replaces the manifest atomically: write a new one, sync it, rename it over the old one.
    fn write_manifest(&self, log: u64) -> io::Result<()> {
        let manifest = Manifest {
            next_id: self.next_id,
            log,
            tables: self.tables.iter().map(|t| t.id).collect(),
        };
        self.fs.create(MANIFEST_TMP)?;
        self.fs
            .append(MANIFEST_TMP, &wal::frame(&manifest.encode()))?;
        self.fs.sync(MANIFEST_TMP)?;
        self.fs.rename(MANIFEST_TMP, MANIFEST)?;
        self.fs.sync_dir()
    }

    /// Deletes logs, tables and manifests that the current manifest doesn't use.
    fn remove_unused(&self) -> io::Result<()> {
        for name in self.fs.list()? {
            let ours = name == MANIFEST_TMP || file_id(&name).is_some();
            let used =
                name == log_name(self.log) || self.tables.iter().any(|t| table_name(t.id) == name);
            if ours && !used {
                self.fs.remove(&name)?;
            }
        }
        Ok(())
    }
}

/// Which tables are live, oldest first, and which log is current.
#[derive(Default)]
struct Manifest {
    next_id: u64,
    log: u64,
    tables: Vec<u64>,
}

impl Manifest {
    fn encode(&self) -> Vec<u8> {
        [self.next_id, self.log]
            .iter()
            .chain(&self.tables)
            .flat_map(|n| n.to_le_bytes())
            .collect()
    }

    fn decode(buf: &[u8]) -> io::Result<Manifest> {
        let (records, len) = wal::records(buf);
        let ([record], true) = (&records[..], len == buf.len()) else {
            return Err(corrupt(MANIFEST));
        };
        let mut r = Reader::new(record);
        let mut manifest = Manifest {
            next_id: r.u64()?,
            log: r.u64()?,
            tables: Vec::new(),
        };
        while !r.done() {
            manifest.tables.push(r.u64()?);
        }
        Ok(manifest)
    }
}

fn corrupt(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{what} is corrupt"))
}

/// Reads little-endian values, failing instead of panicking when the data runs out.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let bytes = self.buf[self.pos..]
            .get(..n)
            .ok_or_else(|| corrupt("a record"))?;
        self.pos += n;
        Ok(bytes)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = &self.buf[self.pos..];
        self.pos = self.buf.len();
        rest
    }

    fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
}
