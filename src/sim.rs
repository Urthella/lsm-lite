//! A filesystem in memory that can crash, for testing that a program keeps what it synced.
//!
//! When it crashes:
//! - Data appended to a file since its last `sync` survives up to a random point, and what
//!   survives may end in zeros, as when a file's new size reached the disk before its data did.
//! - Each create, rename and remove since the last `sync_dir` survives or not, independently, so
//!   they can come back out of order. A rename is still atomic.
//! - A file's data can only be reached through a name that survived.
//!
//! That's harsher than most real filesystems, which is the point.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use crate::Fs;

/// Clones share one filesystem, so a test can keep a handle while the database owns another.
#[derive(Clone)]
pub struct SimFs {
    state: Arc<Mutex<State>>,
}

struct State {
    files: Vec<File>,
    names: BTreeMap<String, usize>,
    durable_names: BTreeMap<String, usize>,
    pending: Vec<DirOp>,
    ops: u64,
    fail_at: Option<u64>,
    rng: u64,
}

struct File {
    data: Vec<u8>,
    synced: usize,
}

enum DirOp {
    Link(String, usize),
    Rename(String, String, usize),
    Unlink(String),
}

impl SimFs {
    /// The seed decides what survives each crash.
    pub fn new(seed: u64) -> SimFs {
        let state = State {
            files: Vec::new(),
            names: BTreeMap::new(),
            durable_names: BTreeMap::new(),
            pending: Vec::new(),
            ops: 0,
            fail_at: None,
            rng: seed,
        };
        SimFs {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Makes operations fail from the `n`th one from now, as if the process died there. `None`
    /// cancels that.
    pub fn fail_after(&self, n: Option<u64>) {
        let mut s = self.state.lock().unwrap();
        s.fail_at = n.map(|n| s.ops + n);
    }

    /// How many operations have succeeded.
    pub fn ops(&self) -> u64 {
        self.state.lock().unwrap().ops
    }

    /// Loses what a crash could lose, then lets operations succeed again.
    pub fn crash(&self) {
        let mut guard = self.state.lock().unwrap();
        let s = &mut *guard;
        for i in 0..s.files.len() {
            let (len, synced) = (s.files[i].data.len(), s.files[i].synced);
            let keep = synced + s.below(len - synced + 1);
            let zero_from = if s.below(2) == 0 {
                synced + s.below(keep - synced + 1)
            } else {
                keep
            };
            let file = &mut s.files[i];
            file.data.truncate(keep);
            file.data[zero_from..].fill(0);
            file.synced = keep;
        }
        let mut names = s.durable_names.clone();
        for op in std::mem::take(&mut s.pending) {
            if s.below(2) == 0 {
                continue;
            }
            match op {
                DirOp::Link(name, file) => {
                    names.insert(name, file);
                }
                DirOp::Rename(from, to, file) => {
                    names.remove(&from);
                    names.insert(to, file);
                }
                DirOp::Unlink(name) => {
                    names.remove(&name);
                }
            }
        }
        s.names = names.clone();
        s.durable_names = names;
        s.fail_at = None;
    }

    fn op(&self) -> io::Result<std::sync::MutexGuard<'_, State>> {
        let mut s = self.state.lock().unwrap();
        if s.fail_at.is_some_and(|at| s.ops >= at) {
            return Err(io::Error::other("simulated crash"));
        }
        s.ops += 1;
        Ok(s)
    }
}

impl State {
    fn below(&mut self, n: usize) -> usize {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        ((z ^ (z >> 31)) % n as u64) as usize
    }

    fn file(&mut self, name: &str) -> io::Result<&mut File> {
        let i = *self.names.get(name).ok_or_else(|| not_found(name))?;
        Ok(&mut self.files[i])
    }
}

fn not_found(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, name.to_string())
}

impl Fs for SimFs {
    fn create(&self, name: &str) -> io::Result<()> {
        let mut s = self.op()?;
        s.files.push(File {
            data: Vec::new(),
            synced: 0,
        });
        let file = s.files.len() - 1;
        s.names.insert(name.to_string(), file);
        s.pending.push(DirOp::Link(name.to_string(), file));
        Ok(())
    }

    fn append(&self, name: &str, data: &[u8]) -> io::Result<()> {
        self.op()?.file(name)?.data.extend_from_slice(data);
        Ok(())
    }

    fn sync(&self, name: &str) -> io::Result<()> {
        let mut s = self.op()?;
        let file = s.file(name)?;
        file.synced = file.data.len();
        Ok(())
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        Ok(self.op()?.file(name)?.data.clone())
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut s = self.op()?;
        let data = &s.file(name)?.data;
        let range = data.get(offset as usize..).and_then(|rest| rest.get(..len));
        range
            .map(<[u8]>::to_vec)
            .ok_or_else(|| io::ErrorKind::UnexpectedEof.into())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut s = self.op()?;
        let file = s.names.remove(from).ok_or_else(|| not_found(from))?;
        s.names.insert(to.to_string(), file);
        s.pending
            .push(DirOp::Rename(from.to_string(), to.to_string(), file));
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let mut s = self.op()?;
        s.names.remove(name).ok_or_else(|| not_found(name))?;
        s.pending.push(DirOp::Unlink(name.to_string()));
        Ok(())
    }

    fn list(&self) -> io::Result<Vec<String>> {
        Ok(self.op()?.names.keys().cloned().collect())
    }

    fn sync_dir(&self) -> io::Result<()> {
        let mut s = self.op()?;
        s.durable_names = s.names.clone();
        s.pending.clear();
        Ok(())
    }
}
